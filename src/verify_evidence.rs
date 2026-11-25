//! Pure terminal-failure extraction, comparison, and formatting for verification commands.
//!
//! When `graph::run_e2e_with_retry` runs a failed `verify.e2e` command a second time on
//! the same commit and both attempts exit non-zero, exiting non-zero twice proves only
//! that verification stays red — not that the same test failed twice. This module parses
//! raw command output (before `run::summarize_verify` reduces it) to extract confirmed,
//! count-reconciled terminal test failures, separates intermediate `TRY` failures,
//! `FLAKY` passes, and cancellations from terminal verdicts, compares both attempts
//! conservatively (`Matching`, `Distinct`, or `Unknown`), and formats the saved record
//! consistently across events, CLI/web reports, fixer prompts, recovery evidence, stop
//! reasons, and automatic task chat reports.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::run::{CommandOutcome, E2eStatus, RunState, summarize_verify};

/// Maximum number of terminal failures captured in a confirmed set. Exceeding this
/// yields [`FailureExtraction::Unknown`] so truncated sets are never compared as complete.
pub const MAX_TERMINAL_FAILURES: usize = 32;

/// Maximum number of test names retained per intermediate category (`TRY`, `FLAKY`, etc.).
pub const MAX_INTERMEDIATE_NAMES: usize = 8;

/// Maximum byte length of the bounded excerpt stored per attempt inside [`RerunComparison`].
pub const ATTEMPT_EXCERPT_MAX: usize = 4_000;

/// Maximum number of failure names listed inline in one-line summaries before `+N more`.
pub const DISPLAY_FAILURE_LIMIT: usize = 6;

/// Legacy prefix previously injected into `output_tail` on double failures.
/// Older `run.json` records carrying this prefix have no structured attempt comparison
/// and must be treated as an unknown comparison rather than proof of a reproduced failure.
pub const LEGACY_FAILED_TWICE_PREFIX: &str =
    "[this command failed twice on the same commit; the output below is the second attempt's]";

/// One confirmed terminal test failure (optional test binary/suite + test name).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TerminalFailure {
    /// Test binary or target identifier when known (e.g. `magi-cli::isolated_build_cache`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    /// Test name within the suite.
    pub test: String,
}

impl TerminalFailure {
    /// Human-readable display label (`<suite> <test>` or `<test>`).
    pub fn display_name(&self) -> String {
        match &self.suite {
            Some(suite) if !suite.is_empty() => format!("{suite} {}", self.test),
            _ => self.test.clone(),
        }
    }
}

/// Intermediate / non-terminal test activity observed in raw output, kept strictly
/// separate from terminal failures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntermediateNotes {
    /// Number of intermediate retry failure lines (`TRY N FAIL` / `TRY N TIMEOUT` / ...).
    #[serde(default)]
    pub try_failure_count: usize,
    /// Bounded sample of test identifiers seen in `TRY N` failure lines.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub try_failures: Vec<String>,
    /// Number of tests that passed on retry (`FLAKY`).
    #[serde(default)]
    pub flaky_count: usize,
    /// Bounded sample of test identifiers marked `FLAKY`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flaky: Vec<String>,
    /// Whether the test runner reported cancellations or partial execution (`M/N tests run`).
    #[serde(default)]
    pub cancelled: bool,
    /// Number of tests not run due to cancellation, when reported in the summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_run_count: Option<usize>,
    /// Bounded sample of long-running test notices (`has been running for over ...`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub long_running: Vec<String>,
}

impl IntermediateNotes {
    /// True when no intermediate retries, flaky passes, cancellations, or hangs were observed.
    pub fn is_empty(&self) -> bool {
        self.try_failure_count == 0
            && self.try_failures.is_empty()
            && self.flaky_count == 0
            && self.flaky.is_empty()
            && !self.cancelled
            && self.not_run_count.is_none()
            && self.long_running.is_empty()
    }

    fn record_try(&mut self, label: String) {
        self.try_failure_count += 1;
        push_bounded_unique(&mut self.try_failures, label, MAX_INTERMEDIATE_NAMES);
    }

    fn record_flaky(&mut self, label: String) {
        if !self.flaky.contains(&label) {
            self.flaky_count += 1;
            push_bounded_unique(&mut self.flaky, label, MAX_INTERMEDIATE_NAMES);
        }
    }

    fn record_long_running(&mut self, label: String) {
        push_bounded_unique(&mut self.long_running, label, MAX_INTERMEDIATE_NAMES);
    }

    /// Compact human-readable summary of separated non-terminal activity, or `None` when empty.
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.try_failure_count > 0 {
            if self.try_failures.is_empty() {
                parts.push(format!(
                    "{} intermediate TRY failure(s)",
                    self.try_failure_count
                ));
            } else {
                parts.push(format!(
                    "{} intermediate TRY failure(s) ({})",
                    self.try_failure_count,
                    join_bounded(&self.try_failures, 3)
                ));
            }
        }
        if self.flaky_count > 0 {
            if self.flaky.is_empty() {
                parts.push(format!("{} FLAKY pass(es)", self.flaky_count));
            } else {
                parts.push(format!(
                    "{} FLAKY pass(es) ({})",
                    self.flaky_count,
                    join_bounded(&self.flaky, 3)
                ));
            }
        }
        if self.cancelled {
            match self.not_run_count {
                Some(n) if n > 0 => parts.push(format!("cancelled ({n} test(s) not run)")),
                _ => parts.push("cancelled".to_owned()),
            }
        }
        if !self.long_running.is_empty() {
            parts.push(format!(
                "long-running test(s): {}",
                join_bounded(&self.long_running, 3)
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// Result of extracting terminal test failures from one attempt's raw output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureExtraction {
    /// Complete, count-reconciled, non-empty set of terminal failing tests.
    Confirmed {
        /// Sorted, deduplicated terminal failing tests.
        failures: Vec<TerminalFailure>,
    },
    /// Terminal failing tests could not be confirmed completely.
    Unknown {
        /// Why completeness could not be confirmed.
        reason: String,
    },
}

/// Extracted terminal failure set paired with separated intermediate activity for one attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureEvidence {
    /// Confirmed terminal failures or why they are unknown.
    pub extraction: FailureExtraction,
    /// Intermediate retries (`TRY`), `FLAKY` passes, and cancellations, kept separate.
    #[serde(default)]
    pub intermediate: IntermediateNotes,
}

/// Bounded evidence preserved for one verification attempt in a two-attempt rerun pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptEvidence {
    /// 1-based attempt number (`1` = first failure, `2` = rerun).
    pub attempt: u8,
    /// Process exit code, or `None` on timeout/signal/spawn error.
    pub code: Option<i32>,
    /// Wall-clock duration in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
    /// Run-relative artifact path (e.g. `artifacts/e2e-3.out`), when saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// Bounded output excerpt (within [`ATTEMPT_EXCERPT_MAX`]).
    #[serde(default)]
    pub excerpt: String,
    /// Terminal failure extraction from raw output.
    pub extraction: FailureExtraction,
    /// Intermediate retries, flaky passes, and cancellations, kept separate.
    #[serde(default)]
    pub intermediate: IntermediateNotes,
}

impl AttemptEvidence {
    /// Build bounded attempt evidence from a finished [`CommandOutcome`].
    pub fn from_outcome(attempt: u8, outcome: &CommandOutcome) -> Self {
        let (extraction, intermediate) = match &outcome.failure_evidence {
            Some(ev) => (ev.extraction.clone(), ev.intermediate.clone()),
            None => (
                FailureExtraction::Unknown {
                    reason: "no raw failure extraction recorded".to_owned(),
                },
                IntermediateNotes::default(),
            ),
        };
        Self {
            attempt,
            code: outcome.code,
            duration_ms: outcome.duration_ms,
            artifact: outcome.artifact.clone(),
            excerpt: summarize_verify(&outcome.output_tail, ATTEMPT_EXCERPT_MAX),
            extraction,
            intermediate,
        }
    }

    /// Return a copy with `excerpt` scrubbed through `scrub`.
    pub fn scrubbed(&self, scrub: impl Fn(&str) -> String) -> Self {
        let mut copy = self.clone();
        copy.excerpt = scrub(&copy.excerpt);
        copy
    }
}

/// Conservative comparison verdict between two consecutive failing attempts on one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ComparisonVerdict {
    /// Both attempts had confirmed, non-empty, identical terminal test failure sets.
    Matching {
        /// Confirmed terminal failures present in both attempts.
        failures: Vec<TerminalFailure>,
    },
    /// Both attempts had confirmed, non-empty, differing terminal test failure sets.
    Distinct {
        /// Terminal failures present in both attempts.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        common: Vec<TerminalFailure>,
        /// Terminal failures present only in the first attempt.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        first_only: Vec<TerminalFailure>,
        /// Terminal failures present only in the second attempt.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        second_only: Vec<TerminalFailure>,
    },
    /// At least one attempt's terminal test failures could not be confirmed.
    Unknown {
        /// Why the two attempts could not be compared conclusively.
        reason: String,
    },
}

/// Structured record persisted on a [`CommandOutcome`] that failed twice on one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RerunComparison {
    /// Commit SHA both attempts ran against.
    pub head: String,
    /// First failing attempt's bounded evidence.
    pub first: AttemptEvidence,
    /// Second failing attempt's bounded evidence.
    pub second: AttemptEvidence,
    /// Conservative comparison verdict (`Matching`, `Distinct`, or `Unknown`).
    pub verdict: ComparisonVerdict,
}

impl RerunComparison {
    /// Return a copy with attempt excerpts scrubbed through `scrub`.
    pub fn scrubbed(&self, scrub: impl Fn(&str) -> String) -> Self {
        Self {
            head: self.head.clone(),
            first: self.first.scrubbed(&scrub),
            second: self.second.scrubbed(&scrub),
            verdict: self.verdict.clone(),
        }
    }
}

/// True when `output_tail` begins with the legacy double-failure prefix that predates
/// structured [`RerunComparison`].
pub fn legacy_failed_twice(output_tail: &str) -> bool {
    output_tail
        .trim_start()
        .starts_with(LEGACY_FAILED_TWICE_PREFIX)
}

/// Strip ANSI CSI/OSC escape sequences and trailing `\r` from one line.
pub fn strip_ansi(line: &str) -> String {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Extract terminal test failures and intermediate notes from separate raw `stdout` and
/// `stderr` streams. Neither stream's order is assumed to imply wall-clock execution order.
pub fn extract_streams(
    code: Option<i32>,
    build_failed: bool,
    stdout: &str,
    stderr: &str,
) -> FailureEvidence {
    let out_lines: Vec<String> = stdout.lines().map(strip_ansi).collect();
    let err_lines: Vec<String> = stderr.lines().map(strip_ansi).collect();
    extract_line_streams(code, build_failed, &[&out_lines, &err_lines])
}

/// Extract terminal test failures and intermediate notes from a single combined raw output.
pub fn extract_combined(code: Option<i32>, build_failed: bool, body: &str) -> FailureEvidence {
    let lines: Vec<String> = body.lines().map(strip_ansi).collect();
    extract_line_streams(code, build_failed, &[&lines])
}

fn extract_line_streams(
    code: Option<i32>,
    build_failed: bool,
    streams: &[&[String]],
) -> FailureEvidence {
    let mut intermediate = IntermediateNotes::default();
    let mut libtest_total = LibtestSummary::default();
    let mut nextest_total = NextestSummary::default();

    for lines in streams {
        let (lib, captured_ranges) = parse_libtest_stream(lines, &mut intermediate);
        libtest_total.merge(lib);
        let nx = parse_nextest_stream(lines, &captured_ranges, &mut intermediate);
        nextest_total.merge(nx);
    }

    // Cargo writes binary announcements and libtest verdicts to different pipes.
    // Without interleaving information, only a single announcement and a single
    // libtest run can be associated unambiguously.
    if libtest_total.runs == 1 && libtest_total.suites == 1 {
        for failure in &mut libtest_total.failures {
            if failure.suite.is_none() {
                failure.suite.clone_from(&libtest_total.only_suite);
            }
        }
    }

    let extraction = if code.is_none() {
        FailureExtraction::Unknown {
            reason: "command timed out or terminated without an exit code".to_owned(),
        }
    } else if code == Some(0) {
        FailureExtraction::Unknown {
            reason: "command exited 0".to_owned(),
        }
    } else if build_failed {
        FailureExtraction::Unknown {
            reason: "command failed to build or link".to_owned(),
        }
    } else if libtest_total.incomplete || nextest_total.incomplete {
        FailureExtraction::Unknown {
            reason: "test run did not finish with a final summary".to_owned(),
        }
    } else if libtest_total.overflow || nextest_total.overflow {
        FailureExtraction::Unknown {
            reason: format!(
                "more than {MAX_TERMINAL_FAILURES} terminal test failures reported; complete comparison unavailable"
            ),
        }
    } else if libtest_total.inconsistent || nextest_total.inconsistent {
        FailureExtraction::Unknown {
            reason: "reported failure count did not match listed terminal failures".to_owned(),
        }
    } else if libtest_total.failed_runs > 0 && nextest_total.failed_runs > 0 {
        FailureExtraction::Unknown {
            reason: "multiple test runners reported failures in one command; completeness cannot be confirmed"
                .to_owned(),
        }
    } else if nextest_total.failed_runs > 0 {
        finalize_confirmed(nextest_total.expected_failures, nextest_total.failures)
    } else if libtest_total.failed_runs > 0 {
        finalize_confirmed(libtest_total.expected_failures, libtest_total.failures)
    } else {
        FailureExtraction::Unknown {
            reason: "command exited non-zero without a complete failing test summary".to_owned(),
        }
    };

    FailureEvidence {
        extraction,
        intermediate,
    }
}

fn finalize_confirmed(expected: usize, raw: Vec<TerminalFailure>) -> FailureExtraction {
    if expected == 0 || raw.is_empty() {
        return FailureExtraction::Unknown {
            reason: "no terminal test failures were listed in the final summary".to_owned(),
        };
    }
    if expected > MAX_TERMINAL_FAILURES || raw.len() > MAX_TERMINAL_FAILURES {
        return FailureExtraction::Unknown {
            reason: format!(
                "more than {MAX_TERMINAL_FAILURES} terminal test failures reported; complete comparison unavailable"
            ),
        };
    }
    let set: BTreeSet<TerminalFailure> = raw.into_iter().collect();
    if set.len() != expected {
        return FailureExtraction::Unknown {
            reason: format!(
                "reported failure count ({expected}) did not match distinct terminal failures ({})",
                set.len()
            ),
        };
    }
    FailureExtraction::Confirmed {
        failures: set.into_iter().collect(),
    }
}

/// Compare two consecutive failing attempts of the same command on commit `head`.
pub fn compare_attempts(
    head: &str,
    first: &CommandOutcome,
    second: &CommandOutcome,
) -> RerunComparison {
    let first_ev = AttemptEvidence::from_outcome(1, first);
    let second_ev = AttemptEvidence::from_outcome(2, second);
    let verdict = match (&first_ev.extraction, &second_ev.extraction) {
        (
            FailureExtraction::Confirmed { failures: f1 },
            FailureExtraction::Confirmed { failures: f2 },
        ) if !f1.is_empty() && !f2.is_empty() => {
            if f1.iter().chain(f2).any(|f| f.suite.is_none()) {
                ComparisonVerdict::Unknown {
                    reason: "terminal failure has no confirmed test binary".to_owned(),
                }
            } else if f1 == f2 {
                ComparisonVerdict::Matching {
                    failures: f1.clone(),
                }
            } else {
                let s1: BTreeSet<&TerminalFailure> = f1.iter().collect();
                let s2: BTreeSet<&TerminalFailure> = f2.iter().collect();
                let common = s1.intersection(&s2).map(|f| (*f).clone()).collect();
                let first_only = s1.difference(&s2).map(|f| (*f).clone()).collect();
                let second_only = s2.difference(&s1).map(|f| (*f).clone()).collect();
                ComparisonVerdict::Distinct {
                    common,
                    first_only,
                    second_only,
                }
            }
        }
        (FailureExtraction::Unknown { reason: r1 }, FailureExtraction::Unknown { reason: r2 }) => {
            ComparisonVerdict::Unknown {
                reason: if r1 == r2 {
                    format!("both attempts: {r1}")
                } else {
                    format!("attempt 1: {r1}; attempt 2: {r2}")
                },
            }
        }
        (FailureExtraction::Unknown { reason }, _) => ComparisonVerdict::Unknown {
            reason: format!("attempt 1: {reason}"),
        },
        (_, FailureExtraction::Unknown { reason }) => ComparisonVerdict::Unknown {
            reason: format!("attempt 2: {reason}"),
        },
        _ => ComparisonVerdict::Unknown {
            reason: "empty confirmed terminal failure set".to_owned(),
        },
    };
    RerunComparison {
        head: head.to_owned(),
        first: first_ev,
        second: second_ev,
        verdict,
    }
}

// ---------------------------------------------------------------------------
// Libtest parser
// ---------------------------------------------------------------------------

#[derive(Default)]
struct LibtestSummary {
    runs: usize,
    suites: usize,
    only_suite: Option<String>,
    failed_runs: usize,
    expected_failures: usize,
    failures: Vec<TerminalFailure>,
    incomplete: bool,
    inconsistent: bool,
    overflow: bool,
}

impl LibtestSummary {
    fn merge(&mut self, other: Self) {
        self.runs += other.runs;
        self.suites += other.suites;
        if other.only_suite.is_some() {
            self.only_suite = other.only_suite;
        }
        self.failed_runs += other.failed_runs;
        self.expected_failures += other.expected_failures;
        self.failures.extend(other.failures);
        self.incomplete |= other.incomplete;
        self.inconsistent |= other.inconsistent;
        self.overflow |= other.overflow;
    }
}

struct ActiveLibtest {
    expected_total: usize,
    suite: Option<String>,
    live_failed: BTreeSet<String>,
    in_failures_section: bool,
    captured_start: Option<usize>,
    collecting_list: bool,
    current_list: Vec<String>,
    last_valid_closing: Option<(usize, usize, Vec<String>)>,
    saw_overflow: bool,
}

struct LibtestResultLine {
    ok: bool,
    passed: usize,
    failed: usize,
    ignored: usize,
    measured: usize,
}

impl LibtestResultLine {
    fn total(&self) -> usize {
        self.passed + self.failed + self.ignored + self.measured
    }
}

fn parse_libtest_stream(
    lines: &[String],
    intermediate: &mut IntermediateNotes,
) -> (LibtestSummary, Vec<(usize, usize)>) {
    let mut summary = LibtestSummary::default();
    let mut captured_ranges: Vec<(usize, usize)> = Vec::new();
    let mut pending_suite: Option<String> = None;
    let mut active: Option<ActiveLibtest> = None;
    let mut standalone_collecting = false;
    let mut standalone_list: Vec<String> = Vec::new();
    let mut nextest_captured = false;

    let commit_active = |run: ActiveLibtest,
                         summary: &mut LibtestSummary,
                         captured_ranges: &mut Vec<(usize, usize)>| {
        if run.saw_overflow {
            summary.overflow = true;
        } else if let Some((end_line, expected_f, list)) = run.last_valid_closing {
            if let Some(start) = run.captured_start {
                captured_ranges.push((start, end_line));
            }
            summary.failed_runs += 1;
            summary.expected_failures += expected_f;
            for test in list {
                summary.failures.push(TerminalFailure {
                    suite: run.suite.clone(),
                    test,
                });
            }
        } else {
            summary.incomplete = true;
        }
    };

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();

        // If an active libtest run already found a valid closing summary and a new
        // top-level runner boundary begins, close the previous run first.
        if let Some(run) = active.as_ref()
            && run.in_failures_section
            && run.last_valid_closing.is_some()
            && (parse_libtest_running_header(trimmed).is_some()
                || parse_cargo_running_suite(trimmed).is_some()
                || parse_nextest_summary_header(trimmed).is_some()
                || trimmed == "------------")
            && let Some(finished) = active.take()
        {
            commit_active(finished, &mut summary, &mut captured_ranges);
        }

        if active.is_none() {
            if is_nextest_captured_header(trimmed) {
                nextest_captured = true;
                continue;
            }
            if nextest_captured {
                if trimmed == "------------" {
                    nextest_captured = false;
                } else {
                    continue;
                }
            }
        }

        if let Some(run) = active.as_mut() {
            if !run.in_failures_section {
                if trimmed == "failures:" && !line.starts_with(' ') {
                    run.in_failures_section = true;
                    run.captured_start = Some(idx);
                    run.collecting_list = true;
                    run.current_list.clear();
                } else if let Some(name) = parse_libtest_live_failed(trimmed) {
                    run.live_failed.insert(name);
                } else if let Some(name) = parse_libtest_long_running(trimmed) {
                    intermediate.record_long_running(name);
                } else if let Some(res) = parse_libtest_result(trimmed) {
                    if res.ok
                        && res.failed == 0
                        && res.total() == run.expected_total
                        && run.live_failed.is_empty()
                    {
                        active = None;
                    } else {
                        summary.inconsistent = true;
                        active = None;
                    }
                } else if parse_libtest_running_header(trimmed).is_some()
                    || parse_nextest_summary_header(trimmed).is_some()
                {
                    if let Some(unfinished) = active.take() {
                        commit_active(unfinished, &mut summary, &mut captured_ranges);
                    }
                    if let Some(total) = parse_libtest_running_header(trimmed) {
                        summary.runs += 1;
                        active = Some(new_active_libtest(total, pending_suite.take()));
                    }
                }
            } else {
                // Inside failures: section (captured output + final failures list).
                if trimmed == "failures:" && !line.starts_with(' ') {
                    run.collecting_list = true;
                    run.current_list.clear();
                } else if run.collecting_list {
                    if line.starts_with("    ")
                        && !trimmed.is_empty()
                        && !trimmed.starts_with("---- ")
                    {
                        if run.current_list.len() <= MAX_TERMINAL_FAILURES {
                            run.current_list.push(trimmed.to_owned());
                        }
                    } else if trimmed.is_empty() {
                        run.collecting_list = false;
                    } else if let Some(res) = parse_libtest_result(trimmed) {
                        run.collecting_list = false;
                        try_close_libtest(run, idx, &res);
                    } else {
                        run.collecting_list = false;
                        run.current_list.clear();
                    }
                } else if let Some(res) = parse_libtest_result(trimmed) {
                    try_close_libtest(run, idx, &res);
                }
            }
            continue;
        }

        // Idle state (outside an active `running N tests` block).
        if let Some(suite) = parse_cargo_running_suite(trimmed) {
            summary.suites += 1;
            summary.only_suite = Some(suite.clone());
            pending_suite = Some(suite);
            standalone_collecting = false;
            standalone_list.clear();
        } else if let Some(total) = parse_libtest_running_header(trimmed) {
            summary.runs += 1;
            standalone_collecting = false;
            standalone_list.clear();
            active = Some(new_active_libtest(total, pending_suite.take()));
        } else if trimmed == "failures:" && !line.starts_with(' ') {
            standalone_collecting = true;
            standalone_list.clear();
        } else if standalone_collecting {
            if line.starts_with("    ") && !trimmed.is_empty() && !trimmed.starts_with("---- ") {
                if standalone_list.len() <= MAX_TERMINAL_FAILURES {
                    standalone_list.push(trimmed.to_owned());
                }
            } else if trimmed.is_empty() {
                standalone_collecting = false;
            } else if let Some(res) = parse_libtest_result(trimmed) {
                standalone_collecting = false;
                record_standalone_libtest(&res, &standalone_list, &mut summary);
                standalone_list.clear();
            } else {
                standalone_collecting = false;
                standalone_list.clear();
            }
        } else if let Some(res) = parse_libtest_result(trimmed) {
            record_standalone_libtest(&res, &standalone_list, &mut summary);
            standalone_list.clear();
            pending_suite = None;
        } else if let Some(name) = parse_libtest_long_running(trimmed) {
            intermediate.record_long_running(name);
        }
    }

    if let Some(run) = active.take() {
        commit_active(run, &mut summary, &mut captured_ranges);
    }

    (summary, captured_ranges)
}

fn new_active_libtest(expected_total: usize, suite: Option<String>) -> ActiveLibtest {
    ActiveLibtest {
        expected_total,
        suite,
        live_failed: BTreeSet::new(),
        in_failures_section: false,
        captured_start: None,
        collecting_list: false,
        current_list: Vec::new(),
        last_valid_closing: None,
        saw_overflow: false,
    }
}

fn try_close_libtest(run: &mut ActiveLibtest, idx: usize, res: &LibtestResultLine) {
    if res.ok || res.failed == 0 || res.total() != run.expected_total {
        return;
    }
    if res.failed > MAX_TERMINAL_FAILURES || run.current_list.len() > MAX_TERMINAL_FAILURES {
        run.saw_overflow = true;
        return;
    }
    if run.current_list.len() != res.failed {
        return;
    }
    if !run.live_failed.is_empty()
        && (run.live_failed.len() != res.failed
            || !run.current_list.iter().all(|n| run.live_failed.contains(n)))
    {
        return;
    }
    run.last_valid_closing = Some((idx, res.failed, run.current_list.clone()));
}

fn record_standalone_libtest(
    res: &LibtestResultLine,
    list: &[String],
    summary: &mut LibtestSummary,
) {
    summary.runs += 1;
    if res.ok && res.failed == 0 {
        return;
    }
    if res.failed > MAX_TERMINAL_FAILURES || list.len() > MAX_TERMINAL_FAILURES {
        summary.overflow = true;
        return;
    }
    if res.failed == 0 || list.len() != res.failed {
        summary.inconsistent = true;
        return;
    }
    summary.failed_runs += 1;
    summary.expected_failures += res.failed;
    for test in list {
        summary.failures.push(TerminalFailure {
            suite: None,
            test: test.clone(),
        });
    }
}

fn parse_cargo_running_suite(trimmed: &str) -> Option<String> {
    if let Some(rest) = trimmed.strip_prefix("Running ")
        && let Some((label, binary)) = rest.split_once(" (")
        && let Some(binary) = binary.strip_suffix(')')
        && let Some(name) = binary.rsplit(['/', '\\']).next()
        && !label.is_empty()
        && !name.is_empty()
    {
        return Some(format!("{label} ({name})"));
    }
    if let Some(rest) = trimmed.strip_prefix("Doc-tests ") {
        let label = rest.trim();
        if !label.is_empty() {
            return Some(format!("doc-tests {label}"));
        }
    }
    None
}

fn parse_libtest_running_header(trimmed: &str) -> Option<usize> {
    let rest = trimmed.strip_prefix("running ")?;
    let num = rest
        .strip_suffix(" tests")
        .or_else(|| rest.strip_suffix(" test"))?;
    num.parse::<usize>().ok()
}

fn parse_libtest_live_failed(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("test ")?;
    let name = rest.strip_suffix(" ... FAILED")?;
    let name = name.strip_suffix(" - should panic").unwrap_or(name).trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn parse_libtest_long_running(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix("test ")?;
    let (name, _) = rest.split_once(" has been running for over ")?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn parse_libtest_result(trimmed: &str) -> Option<LibtestResultLine> {
    let (ok, rest) = if let Some(r) = trimmed.strip_prefix("test result: ok.") {
        (true, r)
    } else {
        (false, trimmed.strip_prefix("test result: FAILED.")?)
    };
    let mut passed = None;
    let mut failed = None;
    let mut ignored = 0usize;
    let mut measured = 0usize;
    for part in rest.split(';') {
        let p = part.trim();
        if let Some(n) = p.strip_suffix(" passed").and_then(|s| s.parse().ok()) {
            passed = Some(n);
        } else if let Some(n) = p.strip_suffix(" failed").and_then(|s| s.parse().ok()) {
            failed = Some(n);
        } else if let Some(n) = p.strip_suffix(" ignored").and_then(|s| s.parse().ok()) {
            ignored = n;
        } else if let Some(n) = p.strip_suffix(" measured").and_then(|s| s.parse().ok()) {
            measured = n;
        }
    }
    Some(LibtestResultLine {
        ok,
        passed: passed?,
        failed: failed?,
        ignored,
        measured,
    })
}

// ---------------------------------------------------------------------------
// Nextest parser
// ---------------------------------------------------------------------------

#[derive(Default)]
struct NextestSummary {
    failed_runs: usize,
    expected_failures: usize,
    failures: Vec<TerminalFailure>,
    incomplete: bool,
    inconsistent: bool,
    overflow: bool,
}

impl NextestSummary {
    fn merge(&mut self, other: Self) {
        self.failed_runs += other.failed_runs;
        self.expected_failures += other.expected_failures;
        self.failures.extend(other.failures);
        self.incomplete |= other.incomplete;
        self.inconsistent |= other.inconsistent;
        self.overflow |= other.overflow;
    }
}

struct NextestHeader {
    expected_failures: usize,
    flaky_count: usize,
    cancelled: bool,
    not_run: Option<usize>,
    unrecognized_failure_token: bool,
}

enum NextestStatusKind {
    TerminalFailure,
    Try,
    Flaky,
    Cancel,
    NonFailure,
}

struct NextestStatusLine {
    kind: NextestStatusKind,
    failure: TerminalFailure,
}

fn in_ranges(idx: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(a, b)| idx >= a && idx <= b)
}

fn parse_nextest_stream(
    lines: &[String],
    libtest_captured: &[(usize, usize)],
    intermediate: &mut IntermediateNotes,
) -> NextestSummary {
    let mut summary = NextestSummary::default();
    let mut in_captured = false;
    let mut started_open = false;
    let mut saw_summary_flaky = 0usize;
    let mut i = 0usize;

    while i < lines.len() {
        if in_ranges(i, libtest_captured) {
            i += 1;
            continue;
        }
        let trimmed = lines[i].trim();

        if is_nextest_captured_header(trimmed) {
            in_captured = true;
            i += 1;
            continue;
        }
        if trimmed == "------------" {
            in_captured = false;
            i += 1;
            continue;
        }
        if in_captured {
            // A genuine nextest cancellation banner or new start ends captured output.
            if trimmed.starts_with("Cancelling due to ")
                || trimmed == "test run cancelled"
                || is_nextest_starting_line(trimmed)
            {
                in_captured = false;
            } else {
                i += 1;
                continue;
            }
        }

        if is_nextest_starting_line(trimmed) {
            started_open = true;
            i += 1;
            continue;
        }

        if trimmed.starts_with("Cancelling due to ") || trimmed == "test run cancelled" {
            intermediate.cancelled = true;
            i += 1;
            continue;
        }

        if let Some(header) = parse_nextest_summary_header(trimmed) {
            started_open = false;
            if header.cancelled {
                intermediate.cancelled = true;
            }
            if let Some(not_run) = header.not_run {
                intermediate.not_run_count =
                    Some(intermediate.not_run_count.unwrap_or(0) + not_run);
            }
            saw_summary_flaky += header.flaky_count;
            if header.unrecognized_failure_token {
                summary.inconsistent = true;
            }

            let mut block_failures = Vec::new();
            let mut j = i + 1;
            while j < lines.len() && !in_ranges(j, libtest_captured) {
                let next_trimmed = lines[j].trim();
                if next_trimmed.is_empty()
                    || next_trimmed == "------------"
                    || next_trimmed.starts_with("error:")
                    || next_trimmed.starts_with("[cargo-make]")
                    || parse_nextest_summary_header(next_trimmed).is_some()
                {
                    break;
                }
                let Some(parsed) = parse_nextest_status_line(next_trimmed) else {
                    break;
                };
                match parsed.kind {
                    NextestStatusKind::TerminalFailure => {
                        if block_failures.len() <= MAX_TERMINAL_FAILURES {
                            block_failures.push(parsed.failure);
                        }
                    }
                    NextestStatusKind::Try => {
                        // Intermediate retry failures inside Summary are strictly separated
                        // from terminal failures.
                        intermediate.record_try(parsed.failure.display_name());
                    }
                    NextestStatusKind::Flaky => {
                        intermediate.record_flaky(parsed.failure.display_name());
                    }
                    NextestStatusKind::Cancel => {
                        intermediate.cancelled = true;
                    }
                    NextestStatusKind::NonFailure => {}
                }
                j += 1;
            }

            if header.expected_failures > MAX_TERMINAL_FAILURES
                || block_failures.len() > MAX_TERMINAL_FAILURES
            {
                summary.overflow = true;
            } else if header.expected_failures == 0 {
                if !block_failures.is_empty() {
                    summary.inconsistent = true;
                }
            } else if block_failures.len() != header.expected_failures {
                summary.inconsistent = true;
            } else {
                summary.failed_runs += 1;
                summary.expected_failures += header.expected_failures;
                summary.failures.extend(block_failures);
            }
            i = j;
            continue;
        }

        // Non-summary status lines during execution: record intermediate TRY / FLAKY / CANCEL
        // only, never terminal failures (only final Summary decides terminal failures).
        if let Some(parsed) = parse_nextest_status_line(trimmed) {
            match parsed.kind {
                NextestStatusKind::Try => intermediate.record_try(parsed.failure.display_name()),
                NextestStatusKind::Flaky => {
                    intermediate.record_flaky(parsed.failure.display_name())
                }
                NextestStatusKind::Cancel => intermediate.cancelled = true,
                NextestStatusKind::TerminalFailure | NextestStatusKind::NonFailure => {}
            }
        }
        i += 1;
    }

    if saw_summary_flaky > intermediate.flaky_count {
        intermediate.flaky_count = saw_summary_flaky;
    }
    if started_open {
        summary.incomplete = true;
    }
    summary
}

fn is_nextest_captured_header(trimmed: &str) -> bool {
    (trimmed.starts_with("--- STDOUT:") || trimmed.starts_with("--- STDERR:"))
        && trimmed.ends_with("---")
}

fn is_nextest_starting_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("Starting ") else {
        return false;
    };
    let mut parts = rest.split_whitespace();
    let Some(count) = parts.next().and_then(|n| n.parse::<usize>().ok()) else {
        return false;
    };
    count > 0 && matches!(parts.next(), Some("test" | "tests"))
}

fn parse_nextest_summary_header(trimmed: &str) -> Option<NextestHeader> {
    let rest = trimmed.strip_prefix("Summary [")?;
    let (dur, after) = rest.split_once(']')?;
    if !is_duration_token(dur.trim()) {
        return None;
    }
    let (counts_part, details) = after.trim().split_once(':')?;
    let run_spec = counts_part
        .trim()
        .strip_suffix(" tests run")
        .or_else(|| counts_part.trim().strip_suffix(" test run"))?;

    let mut cancelled = false;
    let mut not_run = None;
    if let Some((ran, total)) = run_spec.split_once('/') {
        let ran: usize = ran.trim().parse().ok()?;
        let total: usize = total.trim().parse().ok()?;
        if ran < total {
            cancelled = true;
            not_run = Some(total - ran);
        }
    } else {
        run_spec.trim().parse::<usize>().ok()?;
    }

    let mut expected_failures = 0usize;
    let mut flaky_count = 0usize;
    let mut unrecognized_failure_token = false;

    for segment in split_top_level_commas(details) {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        let (main, paren) = match seg.split_once('(') {
            Some((m, p)) => (m.trim(), Some(p.trim_end_matches(')').trim())),
            None => (seg, None),
        };
        if let Some(p) = paren {
            for sub in p.split(',') {
                let sub = sub.trim();
                if let Some(n) = sub
                    .strip_suffix(" flaky")
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    flaky_count += n;
                }
                if sub.contains("cancel") {
                    cancelled = true;
                }
            }
        }
        let mut words = main.splitn(2, char::is_whitespace);
        let Some(n) = words.next().and_then(|s| s.parse::<usize>().ok()) else {
            if main.contains("cancel") {
                cancelled = true;
            }
            continue;
        };
        let label = words.next().unwrap_or("").trim();
        match label {
            "passed" | "skipped" => {}
            "failed" | "timed out" | "exec failed" => expected_failures += n,
            "not run" => {
                if n > 0 {
                    cancelled = true;
                    not_run = Some(not_run.unwrap_or(0).max(n));
                }
            }
            other => {
                let lower = other.to_lowercase();
                if lower.contains("fail") || lower.contains("abort") || lower.contains("timeout") {
                    unrecognized_failure_token = true;
                }
            }
        }
    }

    Some(NextestHeader {
        expected_failures,
        flaky_count,
        cancelled,
        not_run,
        unrecognized_failure_token,
    })
}

fn split_top_level_commas(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in input.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&input[start..idx]);
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn is_duration_token(tok: &str) -> bool {
    let Some(num) = tok.strip_suffix('s') else {
        return false;
    };
    !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

fn parse_nextest_status_line(trimmed: &str) -> Option<NextestStatusLine> {
    let (raw_prefix, after_open) = trimmed.split_once('[')?;
    let (dur, rest) = after_open.split_once(']')?;
    if !is_duration_token(dur.trim()) {
        return None;
    }
    let prefix = raw_prefix.trim();
    let kind = if prefix.starts_with("TRY ") {
        NextestStatusKind::Try
    } else if prefix == "FLAKY" || prefix.starts_with("FLAKY ") {
        NextestStatusKind::Flaky
    } else if matches!(prefix, "CANCEL" | "CANCELLED" | "TERMINATING") {
        NextestStatusKind::Cancel
    } else if matches!(prefix, "PASS" | "SKIP" | "LEAK" | "SLOW" | "START") {
        NextestStatusKind::NonFailure
    } else if matches!(
        prefix,
        "FAIL" | "TIMEOUT" | "ABORT" | "EXECFAIL" | "LEAK-FAIL" | "FAIL+LEAK"
    ) || (prefix.starts_with("SIG")
        && prefix.len() >= 5
        && prefix
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()))
    {
        NextestStatusKind::TerminalFailure
    } else {
        return None;
    };

    let mut rest = rest.trim();
    // Skip optional nextest progress token such as `(1/10)`.
    if let Some(after_paren) = rest.strip_prefix('(')
        && let Some((counter, tail)) = after_paren.split_once(')')
        && let Some((a, b)) = counter.split_once('/')
        && a.bytes().all(|c| c.is_ascii_digit())
        && b.bytes().all(|c| c.is_ascii_digit())
    {
        rest = tail.trim();
    }

    let mut parts = rest.split_whitespace();
    let suite = parts.next()?;
    let test = rest[suite.len()..].trim();
    if suite.is_empty() || test.is_empty() {
        return None;
    }
    Some(NextestStatusLine {
        kind,
        failure: TerminalFailure {
            suite: Some(suite.to_owned()),
            test: test.to_owned(),
        },
    })
}

// ---------------------------------------------------------------------------
// Shared formatting helpers
// ---------------------------------------------------------------------------

fn push_bounded_unique(list: &mut Vec<String>, item: String, max: usize) {
    if list.len() < max && !list.contains(&item) {
        list.push(item);
    }
}

fn join_bounded(items: &[String], max: usize) -> String {
    if items.len() <= max {
        return items.join(", ");
    }
    let shown = items[..max].join(", ");
    format!("{shown} (+{} more)", items.len() - max)
}

fn format_failure_list(failures: &[TerminalFailure], max: usize) -> String {
    let names: Vec<String> = failures.iter().map(TerminalFailure::display_name).collect();
    join_bounded(&names, max)
}

fn format_artifact_pair(c: &RerunComparison) -> Option<String> {
    match (&c.first.artifact, &c.second.artifact) {
        (Some(a), Some(b)) => Some(format!("attempt 1: {a}, attempt 2: {b}")),
        (Some(a), None) => Some(format!("attempt 1: {a}")),
        (None, Some(b)) => Some(format!("attempt 2: {b}")),
        (None, None) => None,
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// Compact one-line summary of a [`RerunComparison`], shared across events, CLI/web
/// reports, stop reasons, and automatic task chat reports.
pub fn comparison_headline(c: &RerunComparison) -> String {
    let head = short_sha(&c.head);
    let artifacts = format_artifact_pair(c)
        .map(|a| format!(" ({a})"))
        .unwrap_or_default();
    match &c.verdict {
        ComparisonVerdict::Matching { failures } => format!(
            "failed twice on {head} with matching terminal failure(s): {}{artifacts}",
            format_failure_list(failures, DISPLAY_FAILURE_LIMIT),
        ),
        ComparisonVerdict::Distinct {
            common,
            first_only,
            second_only,
        } => {
            let mut parts = Vec::new();
            if !first_only.is_empty() {
                parts.push(format!(
                    "attempt 1 only: {}",
                    format_failure_list(first_only, DISPLAY_FAILURE_LIMIT)
                ));
            }
            if !second_only.is_empty() {
                parts.push(format!(
                    "attempt 2 only: {}",
                    format_failure_list(second_only, DISPLAY_FAILURE_LIMIT)
                ));
            }
            if !common.is_empty() {
                parts.push(format!(
                    "common: {}",
                    format_failure_list(common, DISPLAY_FAILURE_LIMIT)
                ));
            }
            format!(
                "failed twice on {head} with distinct terminal failure(s) ({}){artifacts}",
                parts.join("; "),
            )
        }
        ComparisonVerdict::Unknown { reason } => {
            let mut known_sides = Vec::new();
            if let FailureExtraction::Confirmed { failures } = &c.first.extraction {
                known_sides.push(format!(
                    "attempt 1 terminal failure(s): {}",
                    format_failure_list(failures, DISPLAY_FAILURE_LIMIT)
                ));
            }
            if let FailureExtraction::Confirmed { failures } = &c.second.extraction {
                known_sides.push(format!(
                    "attempt 2 terminal failure(s): {}",
                    format_failure_list(failures, DISPLAY_FAILURE_LIMIT)
                ));
            }
            let detail = if known_sides.is_empty() {
                reason.clone()
            } else {
                format!("{reason}; {}", known_sides.join("; "))
            };
            format!(
                "exited non-zero twice on {head}; terminal failure comparison unknown ({detail}){artifacts}"
            )
        }
    }
}

/// Concise comparison or single-attempt summary line for one failing [`CommandOutcome`],
/// if structured evidence or a legacy double-failure marker is present.
pub fn outcome_comparison_summary(o: &CommandOutcome) -> Option<String> {
    if let Some(c) = &o.rerun_comparison {
        return Some(comparison_headline(c));
    }
    if legacy_failed_twice(&o.output_tail) {
        return Some(
            "legacy record: command exited non-zero twice without structured attempt comparison (comparison unknown)"
                .to_owned(),
        );
    }
    if let Some(ev) = &o.failure_evidence {
        match &ev.extraction {
            FailureExtraction::Confirmed { failures } if !failures.is_empty() => {
                let mut line = format!(
                    "terminal failure(s): {}",
                    format_failure_list(failures, DISPLAY_FAILURE_LIMIT)
                );
                if let Some(inter) = ev.intermediate.summary() {
                    line.push_str(&format!("; non-terminal: {inter}"));
                }
                if let Some(art) = &o.artifact {
                    line.push_str(&format!(" ({art})"));
                }
                return Some(line);
            }
            _ => {}
        }
    }
    None
}

/// Format a `verify` event line for one command that failed twice on the same commit.
pub fn rerun_event_message(context: &str, command: &str, c: &RerunComparison) -> String {
    let headline = comparison_headline(c);
    let suffix = match &c.verdict {
        ComparisonVerdict::Matching { .. } => "the failure reproduced — it stands",
        ComparisonVerdict::Distinct { .. } => {
            "failures did not match across attempts — verification stays red"
        }
        ComparisonVerdict::Unknown { .. } => {
            "cannot conclude the same failure reproduced — verification stays red"
        }
    };
    format!("{context}: `{command}` {headline}; {suffix}")
}

/// Format one failed [`CommandOutcome`] for fixer/reviewer/recovery prompts.
pub fn format_command_prompt_section(o: &CommandOutcome, max_output: usize) -> String {
    if let Some(c) = &o.rerun_comparison {
        let mut s = format!("$ {}\n", o.command);
        let _ = writeln!(s, "Rerun comparison: {}", comparison_headline(c));
        match &c.verdict {
            ComparisonVerdict::Matching { .. } => {
                s.push_str(
                    "Both attempts ended with the same confirmed terminal test failure(s).\n",
                );
            }
            ComparisonVerdict::Distinct { .. } => {
                s.push_str(
                    "The two attempts ended with DIFFERENT terminal test failures; do not assume one root cause covers both.\n",
                );
            }
            ComparisonVerdict::Unknown { .. } => {
                s.push_str(
                    "The command exited non-zero twice, but terminal test failures could not be confirmed for both attempts; do not assume the same failure reproduced.\n",
                );
            }
        }
        format_attempt_prompt(&mut s, &c.first, ATTEMPT_EXCERPT_MAX);
        format_attempt_prompt(&mut s, &c.second, max_output);
        return s;
    }
    if legacy_failed_twice(&o.output_tail) {
        let stripped = o
            .output_tail
            .trim_start()
            .strip_prefix(LEGACY_FAILED_TWICE_PREFIX)
            .unwrap_or(&o.output_tail)
            .trim_start();
        return format!(
            "$ {}\n[legacy record: command exited non-zero twice without structured attempt comparison; comparison unknown — do not assume the same test failed twice]\n{}\n",
            o.command,
            summarize_verify(stripped, max_output),
        );
    }
    let mut header = String::new();
    if let Some(art) = &o.artifact
        && !o.output_tail.contains(art)
    {
        let _ = writeln!(header, "[full output: {art}]");
    }
    if let Some(ev) = &o.failure_evidence {
        if let FailureExtraction::Confirmed { failures } = &ev.extraction {
            let _ = writeln!(
                header,
                "[terminal failure(s): {}]",
                format_failure_list(failures, DISPLAY_FAILURE_LIMIT)
            );
        }
        if let Some(inter) = ev.intermediate.summary() {
            let _ = writeln!(header, "[non-terminal activity: {inter}]");
        }
    }
    format!(
        "$ {}\n{}{}\n",
        o.command,
        header,
        summarize_verify(&o.output_tail, max_output),
    )
}

fn format_attempt_prompt(s: &mut String, a: &AttemptEvidence, max_excerpt: usize) {
    let code = a.code.map_or_else(|| "none".to_owned(), |c| c.to_string());
    let art = a
        .artifact
        .as_deref()
        .map(|p| format!(", artifact {p}"))
        .unwrap_or_default();
    let _ = writeln!(
        s,
        "\n--- Attempt {} (exit {code}, {}ms{art}) ---",
        a.attempt, a.duration_ms
    );
    match &a.extraction {
        FailureExtraction::Confirmed { failures } => {
            let _ = writeln!(
                s,
                "Terminal failure(s): {}",
                format_failure_list(failures, MAX_TERMINAL_FAILURES)
            );
        }
        FailureExtraction::Unknown { reason } => {
            let _ = writeln!(s, "Terminal failure(s): unknown ({reason})");
        }
    }
    if let Some(inter) = a.intermediate.summary() {
        let _ = writeln!(s, "Non-terminal activity (separate from verdict): {inter}");
    }
    let excerpt = summarize_verify(&a.excerpt, max_excerpt);
    if !excerpt.trim().is_empty() {
        let _ = writeln!(s, "{excerpt}");
    }
}

/// Format structured details for a failed command in CLI (`magi show`) output.
pub fn format_outcome_cli_lines(o: &CommandOutcome) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(c) = &o.rerun_comparison {
        lines.push(format!("comparison: {}", comparison_headline(c)));
        for a in [&c.first, &c.second] {
            let code = a.code.map_or_else(|| "none".to_owned(), |c| c.to_string());
            let art = a
                .artifact
                .as_deref()
                .map(|p| format!("  {p}"))
                .unwrap_or_default();
            let verdict = match &a.extraction {
                FailureExtraction::Confirmed { failures } => format!(
                    "terminal: {}",
                    format_failure_list(failures, DISPLAY_FAILURE_LIMIT)
                ),
                FailureExtraction::Unknown { reason } => format!("terminal: unknown ({reason})"),
            };
            let inter = a
                .intermediate
                .summary()
                .map(|m| format!("  [non-terminal: {m}]"))
                .unwrap_or_default();
            lines.push(format!(
                "attempt {} (exit {code}, {}ms{art}): {verdict}{inter}",
                a.attempt, a.duration_ms
            ));
        }
    } else {
        if legacy_failed_twice(&o.output_tail) {
            lines.push(
                "comparison: unknown (legacy record exited non-zero twice without structured attempt comparison)"
                    .to_owned(),
            );
        }
        if let Some(art) = &o.artifact {
            lines.push(format!("artifact: {art}"));
        }
        if let Some(ev) = &o.failure_evidence {
            match &ev.extraction {
                FailureExtraction::Confirmed { failures } => lines.push(format!(
                    "terminal failure(s): {}",
                    format_failure_list(failures, DISPLAY_FAILURE_LIMIT)
                )),
                FailureExtraction::Unknown { reason } => {
                    lines.push(format!("terminal failure(s): unknown ({reason})"))
                }
            }
            if let Some(inter) = ev.intermediate.summary() {
                lines.push(format!("non-terminal: {inter}"));
            }
        }
    }
    lines
}

/// Concise summary of the *current* unresolved e2e failure in `state`, if any.
///
/// Respects resolution order: never resurfaces an earlier review round's e2e failure
/// when `head_e2e` or `gate_repair_e2e` subsequently passed, and inspects `head_e2e`,
/// `gate_repair_e2e`, and the latest `ReviewRound` in precedence order.
pub fn current_e2e_failure_summary(state: &RunState) -> Option<String> {
    if let Some(e) = &state.head_e2e {
        if !e.passed {
            if let Some(line) = summarize_failed_outcomes(&e.outcomes) {
                return Some(format!("current head e2e @ {}: {line}", short_sha(&e.head)));
            }
        } else {
            return None;
        }
    }
    if let Some(e) = &state.gate_repair_e2e {
        if !e.passed {
            if let Some(line) = summarize_failed_outcomes(&e.outcomes) {
                return Some(format!("gate repair e2e @ {}: {line}", short_sha(&e.head)));
            }
        } else {
            return None;
        }
    }
    if let Some(r) = state.reviews.last()
        && r.e2e_status() == E2eStatus::Failed
        && let Some(line) = summarize_failed_outcomes(&r.e2e)
    {
        let head = r.verified_head.as_deref().unwrap_or(&r.head);
        return Some(format!(
            "round {} e2e @ {}: {line}",
            r.round,
            short_sha(head)
        ));
    }
    None
}

/// Summarize the first failing non-resource-blocked outcome in `outcomes`, preferring
/// structured rerun comparisons when present.
pub fn summarize_failed_outcomes(outcomes: &[CommandOutcome]) -> Option<String> {
    let failing: Vec<&CommandOutcome> = outcomes
        .iter()
        .filter(|o| !o.ok() && !o.resource_blocked)
        .collect();
    if failing.is_empty() {
        return None;
    }
    let parts: Vec<String> = failing
        .iter()
        .take(2)
        .map(|o| match outcome_comparison_summary(o) {
            Some(detail) => format!("`{}` {detail}", o.command),
            None => format!(
                "`{}` failed (exit {})",
                o.command,
                o.code.map_or_else(|| "none".to_owned(), |c| c.to_string())
            ),
        })
        .collect();
    Some(parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(code: Option<i32>, raw: &str, artifact: Option<&str>) -> CommandOutcome {
        let build_failed = code != Some(0) && raw.contains("error: could not compile");
        let evidence = extract_combined(code, build_failed, raw);
        CommandOutcome {
            command: "cargo make test".to_owned(),
            code,
            output_tail: summarize_verify(raw, crate::run::VERIFY_SUMMARY),
            duration_ms: 1200,
            resource_blocked: false,
            artifact: artifact.map(str::to_owned),
            failure_evidence: Some(evidence),
            rerun_comparison: None,
        }
    }

    #[test]
    fn nextest_separates_terminal_failures_from_try_flaky_and_cancellations() {
        let raw = "\
Starting 12 tests across 3 binaries
\x1b[33m  TRY 1 FAIL\x1b[0m [   0.500s] magi-cli::chat_notify test_flaky_one
\x1b[33m   FLAKY 2/3\x1b[0m [   0.400s] magi-cli::chat_notify test_flaky_one
  TRY 1 FAIL [   1.000s] magi-cli::isolated_build_cache test_cache
  TRY 2 FAIL [   1.000s] magi-cli::isolated_build_cache test_cache
\x1b[31m        FAIL\x1b[0m [   1.200s] magi-cli::isolated_build_cache test_cache
Cancelling due to test failure: 2 tests still running
      CANCEL [   0.000s] magi-cli::web_render test_other
------------
     Summary [  10.500s] 5/12 tests run: 4 passed (1 flaky), 1 failed, 0 skipped
        FAIL [   1.200s] magi-cli::isolated_build_cache test_cache
  TRY 1 FAIL [   1.000s] magi-cli::isolated_build_cache test_cache
   FLAKY 2/3 [   0.400s] magi-cli::chat_notify test_flaky_one
error: test run failed
";
        let ev = extract_combined(Some(100), false, raw);
        assert_eq!(
            ev.extraction,
            FailureExtraction::Confirmed {
                failures: vec![TerminalFailure {
                    suite: Some("magi-cli::isolated_build_cache".to_owned()),
                    test: "test_cache".to_owned(),
                }]
            }
        );
        assert!(ev.intermediate.try_failure_count >= 2);
        assert_eq!(ev.intermediate.flaky_count, 1);
        assert!(ev.intermediate.cancelled);
        assert_eq!(ev.intermediate.not_run_count, Some(7));
    }

    #[test]
    fn typography_0b11_scenario_distinguishes_unit_failure_from_nextest_failure() {
        // Attempt 1: chat unit test failed in cargo test --lib (nextest never ran).
        let attempt1_out = "\
running 2 tests
test ok_test ... ok
test talk::tests::chat_timeout ... FAILED

failures:

---- talk::tests::chat_timeout stdout ----
thread 'talk::tests::chat_timeout' panicked at src/talk.rs:42:9:
timed out waiting for turn

failures:
    talk::tests::chat_timeout

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.01s
";
        let attempt1_err =
            "Running unittests src/lib.rs (target/debug/deps/magi-abc)\nerror: test failed\n";
        let ev1 = extract_streams(Some(101), false, attempt1_out, attempt1_err);

        // Attempt 2: libtest passed on stdout; nextest failed on stderr naming isolated_build_cache.
        let attempt2_out = "\
running 2 tests
test ok_test ... ok
test talk::tests::chat_timeout ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s
";
        let attempt2_err = "\
Running unittests src/lib.rs (target/debug/deps/magi-abc)
Starting 10 tests across 2 binaries
  TRY 1 FAIL [   0.300s] magi-cli::other test_transient
   FLAKY 2/3 [   0.200s] magi-cli::other test_transient
Cancelling due to test failure: 1 test still running
------------
     Summary [  12.000s] 6/10 tests run: 5 passed (1 flaky), 1 failed, 0 skipped
        FAIL [   2.500s] magi-cli::isolated_build_cache test_isolation
error: test run failed
";
        let ev2 = extract_streams(Some(100), false, attempt2_out, attempt2_err);

        let o1 = CommandOutcome {
            command: "cargo make test".to_owned(),
            code: Some(101),
            output_tail: attempt1_out.to_owned(),
            duration_ms: 5000,
            resource_blocked: false,
            artifact: Some("artifacts/e2e-3.out".to_owned()),
            failure_evidence: Some(ev1),
            rerun_comparison: None,
        };
        let o2 = CommandOutcome {
            command: "cargo make test".to_owned(),
            code: Some(100),
            output_tail: attempt2_err.to_owned(),
            duration_ms: 12000,
            resource_blocked: false,
            artifact: Some("artifacts/e2e-4.out".to_owned()),
            failure_evidence: Some(ev2),
            rerun_comparison: None,
        };
        let cmp = compare_attempts("0b11aaaa1234", &o1, &o2);
        assert_eq!(
            cmp.verdict,
            ComparisonVerdict::Distinct {
                common: vec![],
                first_only: vec![TerminalFailure {
                    suite: Some("unittests src/lib.rs (magi-abc)".to_owned()),
                    test: "talk::tests::chat_timeout".to_owned(),
                }],
                second_only: vec![TerminalFailure {
                    suite: Some("magi-cli::isolated_build_cache".to_owned()),
                    test: "test_isolation".to_owned(),
                }],
            }
        );
        let headline = comparison_headline(&cmp);
        assert!(
            headline.contains("distinct terminal failure(s)"),
            "{headline}"
        );
        assert!(headline.contains("talk::tests::chat_timeout"), "{headline}");
        assert!(headline.contains("isolated_build_cache"), "{headline}");
        assert!(headline.contains("artifacts/e2e-3.out"), "{headline}");
        assert!(headline.contains("artifacts/e2e-4.out"), "{headline}");
        let event = rerun_event_message("round 1", "cargo make test", &cmp);
        assert!(!event.contains("reproduced"), "{event}");
    }

    #[test]
    fn hang_without_final_summary_is_unknown_not_matching() {
        let attempt1_out = "\
running 2 tests
test ok_test ... ok
test talk::tests::chat_timeout has been running for over 60 seconds
";
        let ev1 = extract_streams(Some(1), false, attempt1_out, "");
        assert!(matches!(ev1.extraction, FailureExtraction::Unknown { .. }));
        assert_eq!(
            ev1.intermediate.long_running,
            vec!["talk::tests::chat_timeout"]
        );

        let attempt2_err = "\
------------
     Summary [   5.000s] 3 tests run: 2 passed, 1 failed, 0 skipped
        FAIL [   1.000s] magi-cli::isolated_build_cache test_isolation
";
        let ev2 = extract_streams(Some(100), false, "", attempt2_err);
        assert!(matches!(
            ev2.extraction,
            FailureExtraction::Confirmed { .. }
        ));

        let o1 = CommandOutcome {
            command: "cargo make test".to_owned(),
            code: Some(1),
            output_tail: attempt1_out.to_owned(),
            duration_ms: 60_000,
            resource_blocked: false,
            artifact: Some("artifacts/e2e-3.out".to_owned()),
            failure_evidence: Some(ev1),
            rerun_comparison: None,
        };
        let o2 = CommandOutcome {
            command: "cargo make test".to_owned(),
            code: Some(100),
            output_tail: attempt2_err.to_owned(),
            duration_ms: 5_000,
            resource_blocked: false,
            artifact: Some("artifacts/e2e-4.out".to_owned()),
            failure_evidence: Some(ev2),
            rerun_comparison: None,
        };
        let cmp = compare_attempts("deadbeef", &o1, &o2);
        assert!(matches!(cmp.verdict, ComparisonVerdict::Unknown { .. }));
        let headline = comparison_headline(&cmp);
        assert!(headline.contains("comparison unknown"), "{headline}");
        assert!(headline.contains("isolated_build_cache"), "{headline}");
        assert!(headline.contains("artifacts/e2e-3.out"), "{headline}");
        assert!(headline.contains("artifacts/e2e-4.out"), "{headline}");
    }

    #[test]
    fn captured_stdout_cannot_spoof_libtest_or_nextest_verdict() {
        let raw = "\
running 2 tests
test ok_test ... ok
test real_test ... FAILED

failures:

---- real_test stdout ----
failures:
    fake_test

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
------------
     Summary [   0.100s] 1 tests run: 0 passed, 1 failed, 0 skipped
        FAIL [   0.100s] fake::bin fake_nextest

failures:
    real_test

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
";
        let ev = extract_combined(Some(101), false, raw);
        assert_eq!(
            ev.extraction,
            FailureExtraction::Confirmed {
                failures: vec![TerminalFailure {
                    suite: None,
                    test: "real_test".to_owned(),
                }]
            }
        );
    }

    #[test]
    fn count_mismatch_and_cap_overflow_yield_unknown() {
        let mismatch = "\
------------
     Summary [   1.000s] 2 tests run: 0 passed, 2 failed, 0 skipped
        FAIL [   0.500s] pkg::bin only_one
";
        assert!(matches!(
            extract_combined(Some(100), false, mismatch).extraction,
            FailureExtraction::Unknown { .. }
        ));

        let mut many = format!(
            "------------\n     Summary [   1.000s] 40 tests run: 0 passed, 40 failed, 0 skipped\n"
        );
        for i in 0..40 {
            let _ = writeln!(many, "        FAIL [   0.010s] pkg::bin test_{i}");
        }
        assert!(matches!(
            extract_combined(Some(100), false, &many).extraction,
            FailureExtraction::Unknown { .. }
        ));
    }

    #[test]
    fn matching_a_to_a_and_multi_binary_libtest() {
        let raw = "\
Running tests/alpha.rs (target/debug/deps/alpha-1234)
running 2 tests
test a_ok ... ok
test a_fail ... FAILED

failures:
    a_fail

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

Running tests/beta.rs (target/debug/deps/beta-5678)
running 1 test
test b_fail ... FAILED

failures:
    b_fail

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let o1 = outcome(Some(101), raw, Some("artifacts/e2e-1.out"));
        let o2 = outcome(Some(101), raw, Some("artifacts/e2e-2.out"));
        let cmp = compare_attempts("abcdef12", &o1, &o2);
        assert_eq!(
            cmp.verdict,
            ComparisonVerdict::Matching {
                failures: vec![
                    TerminalFailure {
                        suite: Some("tests/alpha.rs (alpha-1234)".to_owned()),
                        test: "a_fail".to_owned(),
                    },
                    TerminalFailure {
                        suite: Some("tests/beta.rs (beta-5678)".to_owned()),
                        test: "b_fail".to_owned(),
                    },
                ]
            }
        );
        let event = rerun_event_message("round 1", "cargo test", &cmp);
        assert!(event.contains("the failure reproduced"), "{event}");
    }

    #[test]
    fn split_cargo_streams_require_unambiguous_binary_identity() {
        let raw = "\
running 1 test
test test_layout ... FAILED

failures:
    test_layout

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let alpha = "Running tests/alpha.rs (target/debug/deps/alpha-1234)\n";
        let beta = "Running tests/beta.rs (target/debug/deps/beta-5678)\n";
        let make = |stderr: &str| {
            let mut o = outcome(Some(101), raw, None);
            o.failure_evidence = Some(extract_streams(Some(101), false, raw, stderr));
            o
        };
        let a = make(alpha);
        assert!(matches!(
            compare_attempts("head", &a, &make(alpha)).verdict,
            ComparisonVerdict::Matching { .. }
        ));
        assert!(matches!(
            compare_attempts("head", &a, &make(beta)).verdict,
            ComparisonVerdict::Distinct { .. }
        ));
        let same_label = "Running tests/alpha.rs (target/debug/deps/alpha-9999)\n";
        assert!(matches!(
            compare_attempts("head", &a, &make(same_label)).verdict,
            ComparisonVerdict::Distinct { .. }
        ));
        let ambiguous = make(&format!("{alpha}{beta}"));
        for unknown in [make(""), ambiguous] {
            assert!(matches!(
                compare_attempts("head", &unknown, &unknown).verdict,
                ComparisonVerdict::Unknown { .. }
            ));
        }
    }

    #[test]
    fn nextest_captured_libtest_failure_is_not_an_independent_run() {
        let raw = "\
Starting 1 test across 1 binary
        FAIL [   0.100s] pkg::bin outer
--- STDOUT: pkg::bin outer ---
running 1 test
test inner ... FAILED

failures:
    inner

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
------------
     Summary [   0.100s] 1 tests run: 0 passed, 1 failed, 0 skipped
        FAIL [   0.100s] pkg::bin outer
error: test run failed
";
        assert_eq!(
            extract_streams(Some(100), false, "", raw).extraction,
            FailureExtraction::Confirmed {
                failures: vec![TerminalFailure {
                    suite: Some("pkg::bin".to_owned()),
                    test: "outer".to_owned(),
                }],
            }
        );
    }

    #[test]
    fn legacy_failed_twice_is_treated_as_unknown() {
        let o = CommandOutcome {
            command: "cargo make test".to_owned(),
            code: Some(1),
            output_tail: format!("{LEGACY_FAILED_TWICE_PREFIX}\nsome output"),
            duration_ms: 100,
            resource_blocked: false,
            artifact: None,
            failure_evidence: None,
            rerun_comparison: None,
        };
        let summary = outcome_comparison_summary(&o).unwrap();
        assert!(summary.contains("comparison unknown"), "{summary}");
        assert!(!summary.contains("reproduced"), "{summary}");
    }
}
