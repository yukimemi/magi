//! Flaky verification: a `verify.e2e` failure that did not reproduce.
//!
//! `graph::run_e2e_with_retry` runs a failed e2e command once more on the same
//! commit before anything is decided from it. When that second run passes, the
//! first failure was not a verdict on the patch, and this module does the two
//! things that follow:
//!
//! - identifies the failure ([`fingerprint`]) without knowing the test
//!   framework, from the failure lines the saved output excerpt keeps;
//! - files one queue task per distinct failure asking for the root cause
//!   ([`record`]), so ten runs hitting the same flaky test leave one task.
//!
//! Best effort by construction, like `followup::after_merge`: a filing failure
//! becomes a run event and never changes a verdict.

use std::path::Path;

use anyhow::{Result, bail};

use crate::queue::{Queue, Source, Task, TaskStatus};
use crate::run::{RunState, VerifyFlake};

/// Node name carried by [`Source::Agent`] on a filed flake task.
pub const NODE: &str = "flake";

/// Lines of normalised failure text a fingerprint is made of, at most.
const SIGNATURE_LINES: usize = 16;

/// Characters of one normalised signature line that count.
const LINE_CLIP: usize = 160;

/// A task already finished for this failure does not cover a new occurrence
/// (the fix did not hold, or something else broke it again); the next free
/// generation is filed. This bounds the scan.
const MAX_GENERATIONS: u32 = 16;

/// A line mentioning one of these (lower-case match) belongs to the failure.
/// Language- and framework-agnostic on purpose: `cargo test`, nextest, pytest,
/// jest, go test and TAP all say at least one of these on a failing test.
const FAILURE_WORDS: &[&str] = &[
    "fail",
    "panick",
    "assert",
    "error",
    "not ok",
    "timed out",
    "timeout",
    "exception",
    "traceback",
    "\u{2717}",
    "\u{2718}",
    "\u{d7}",
];

/// What identifies one failure: the normalised failure lines, sorted and
/// distinct, plus a line to show a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Sorted, distinct, normalised lines (volatile numbers and ids removed).
    pub lines: Vec<String>,
    /// The first failure line as printed, for a title.
    pub label: String,
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.clone().next() == Some('[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Replace what changes between two runs of the same failing test: numbers
/// (durations, line numbers, thread ids, counts), long hex words (hashes) and
/// random temporary directory names. Whitespace is collapsed.
fn normalize(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        let hex = token.len() >= 8 && token.chars().all(|c| c.is_ascii_hexdigit());
        if hex || (token.len() >= 7 && token.starts_with("tmp")) {
            out.push('#');
        } else {
            let mut in_digits = false;
            for c in token.chars() {
                if c.is_ascii_digit() {
                    if !in_digits {
                        out.push('#');
                    }
                    in_digits = true;
                } else {
                    in_digits = false;
                    out.push(c);
                }
            }
        }
        token.clear();
    };
    let mut space = false;
    for c in line.trim().chars() {
        if c.is_alphanumeric() || c == '_' {
            if space {
                out.push(' ');
                space = false;
            }
            token.push(c);
        } else if c.is_whitespace() {
            flush(&mut token, &mut out);
            space = true;
        } else {
            flush(&mut token, &mut out);
            if space {
                out.push(' ');
                space = false;
            }
            out.push(c);
        }
    }
    flush(&mut token, &mut out);
    out.chars().take(LINE_CLIP).collect()
}

fn is_header(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("[full output") || t.starts_with("[... output reduced") || t == "[...]"
}

fn is_failure_line(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    !lower.starts_with("test result: ok") && FAILURE_WORDS.iter().any(|word| lower.contains(word))
}

/// The [`Signature`] of a failing command's output (the excerpt magi kept) and
/// exit code.
///
/// Failure lines (see [`FAILURE_WORDS`]) are normalised and sorted, so the
/// order tests happen to fail in under parallel execution and the numbers a
/// flaky assertion prints do not change the identity. Output with no failure
/// line at all falls back to its last three lines, then to the exit code.
pub fn signature(output: &str, code: Option<i32>) -> Signature {
    let plain: Vec<String> = output
        .lines()
        .filter(|l| !is_header(l))
        .map(strip_ansi)
        .filter(|l| !l.trim().is_empty())
        .collect();
    let failing: Vec<&String> = plain.iter().filter(|l| is_failure_line(l)).collect();
    let (picked, label): (Vec<&String>, String) = if failing.is_empty() {
        let tail: Vec<&String> = plain.iter().rev().take(3).rev().collect();
        let label = tail.first().map(|l| l.trim().to_owned());
        (tail, label.unwrap_or_default())
    } else {
        (failing.clone(), failing[0].trim().to_owned())
    };
    let mut lines: Vec<String> = picked.iter().map(|l| normalize(l)).collect();
    lines.sort();
    lines.dedup();
    lines.truncate(SIGNATURE_LINES);
    if lines.is_empty() {
        lines.push(format!(
            "exit code {}",
            code.map_or("none".into(), |c| c.to_string())
        ));
    }
    let label = if label.is_empty() {
        lines[0].clone()
    } else {
        label.chars().take(100).collect()
    };
    Signature { lines, label }
}

/// A stable identifier of the failure in `repo`: 40 bits of FNV-1a over the
/// repository and the [`Signature`] lines, as ten hex digits. The command is
/// deliberately not part of it: the same failing test reached through a
/// different command line is the same defect.
pub fn fingerprint(repo: &Path, signature: &Signature) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(repo.to_string_lossy().as_bytes());
    for line in &signature.lines {
        feed(&[0]);
        feed(line.as_bytes());
    }
    format!("{:010x}", h & 0xff_ffff_ffff)
}

/// The id of the generation-`n` task for failure `key`. Ends in the key, so
/// `queue::short` names the failure whatever the generation.
fn task_id(key: &str, generation: u32) -> String {
    if generation <= 1 {
        format!("flake-{key}")
    } else {
        format!("flake-r{generation}-{key}")
    }
}

fn marker(key: &str) -> String {
    format!("Failure fingerprint: {key}")
}

fn fenced(text: &str) -> String {
    // A fence longer than any backtick run inside, so the excerpt cannot
    // close it early.
    let longest = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
        .max(2);
    let fence = "`".repeat(longest + 1);
    format!("{fence}\n{}\n{fence}", text.trim_end())
}

fn build_task(state: &RunState, flake: &VerifyFlake, label: &str, id: String) -> Task {
    let excerpt = crate::scrub::scrub(&flake.first_failure, &crate::scrub::Identity::current());
    let instruction = format!(
        "A verification command failed once and then passed when it was run again on the \
         same commit, so magi recorded the failure as a flake instead of charging it to the \
         change that was being verified. Find the root cause of the intermittent failure and \
         fix it.\n\n\
         Fix the cause, not the symptom: do not loosen the assertion, add a retry, raise a \
         timeout or skip the check to make it quieter. Typical causes are a race or an \
         ordering assumption in the code or the test, shared state between tests (a fixed \
         port, a shared directory, process-global state), a time-based wait that is too \
         tight under load, and a layout or timing measurement taken before the page or \
         process settled. To reproduce, run the check repeatedly and with several copies at \
         once. If it no longer fails on current main after a serious attempt, say so and \
         change nothing.\n\n\
         {marker}\n\
         Command: `{command}`\n\
         Seen in run {run} ({context}) on commit {head}\n\n\
         ## First failure (the rerun passed)\n\n{excerpt}\n",
        marker = marker(&flake.fingerprint),
        command = flake.command,
        run = state.id,
        context = flake.context,
        head = flake.head,
        excerpt = fenced(&excerpt),
    );
    let mut task = Task::new(
        crate::queue::title_from(&format!("flaky verification: {label}"), 90),
        instruction,
        state.repo.clone(),
        Source::Agent {
            run: state.id.clone(),
            node: NODE.to_owned(),
        },
    );
    task.id = id;
    task.solo = true;
    task
}

/// File the task for `flake`, or find the unfinished one that already tracks
/// the same failure. Returns the task id and whether this call created it.
///
/// Ids are derived from the failure, never from the run, and written with
/// [`Queue::create_new`], so two runs hitting the same failure concurrently
/// still leave one task. A finished (`Done`) task does not cover a recurrence:
/// the next generation id is tried.
pub fn file(
    state: &RunState,
    flake: &VerifyFlake,
    label: &str,
    queue: &Queue,
) -> Result<(String, bool)> {
    for generation in 1..=MAX_GENERATIONS {
        let id = task_id(&flake.fingerprint, generation);
        let mut task = build_task(state, flake, label, id.clone());
        if queue.create_new(&mut task)? {
            return Ok((id, true));
        }
        let existing = queue.get(&id)?;
        if existing.status != TaskStatus::Done {
            return Ok((id, false));
        }
    }
    bail!("{MAX_GENERATIONS} earlier tasks for this failure are already done")
}

/// Record a flake in the run's ledger and file (or find) its task. Never
/// fails: the outcome is part of the record and of a `verify` event.
pub fn record(state: &mut RunState, mut flake: VerifyFlake, label: &str, queue: &Queue) {
    match file(state, &flake, label, queue) {
        Ok((id, created)) => {
            state.event(
                "verify",
                format!(
                    "{}: `{}` failed, then passed on the same commit {} - a flake, not a \
                     verdict on the change; {} task {id} (failure {})",
                    flake.context,
                    flake.command,
                    short(&flake.head),
                    if created {
                        "filed"
                    } else {
                        "already tracked by"
                    },
                    flake.fingerprint,
                ),
            );
            flake.task = Some(id);
            flake.filed = created;
        }
        Err(e) => {
            state.event(
                "verify",
                format!(
                    "{}: `{}` failed, then passed on the same commit {} - a flake, not a \
                     verdict on the change; no task could be filed: {e:#}",
                    flake.context,
                    flake.command,
                    short(&flake.head),
                ),
            );
            flake.filing_error = Some(format!("{e:#}"));
        }
    }
    state.verify_flakes.push(flake);
}

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(text: &str) -> Signature {
        signature(text, Some(101))
    }

    #[test]
    fn the_same_failure_has_one_signature_whatever_the_numbers_and_order() {
        let a = "[full output: artifacts/e2e-1.out]\n\
                 running 3 tests\n\
                 test web::tests::layout ... FAILED\n\
                 test talk::tests::turn ... FAILED\n\
                 thread 'web::tests::layout' panicked at src/web.rs:101:9:\n\
                 timed out after 5.31s waiting for 17 rows\n\
                 test result: FAILED. 1 passed; 2 failed; 0 ignored; finished in 6.02s\n";
        let b = "[full output: artifacts/e2e-4.out]\n\
                 running 3 tests\n\
                 test talk::tests::turn ... FAILED\n\
                 thread 'web::tests::layout' panicked at src/web.rs:101:9:\n\
                 timed out after 7.9s waiting for 4 rows\n\
                 test web::tests::layout ... FAILED\n\
                 test result: FAILED. 1 passed; 2 failed; 0 ignored; finished in 8.11s\n";
        assert_eq!(sig(a).lines, sig(b).lines);
        let repo = Path::new("/repo");
        assert_eq!(fingerprint(repo, &sig(a)), fingerprint(repo, &sig(b)));
    }

    #[test]
    fn a_different_failing_test_is_a_different_failure() {
        let a = "test web::tests::layout ... FAILED\n";
        let b = "test talk::tests::turn ... FAILED\n";
        let repo = Path::new("/repo");
        assert_ne!(fingerprint(repo, &sig(a)), fingerprint(repo, &sig(b)));
        assert_ne!(
            fingerprint(Path::new("/one"), &sig(a)),
            fingerprint(Path::new("/two"), &sig(a)),
            "the same text in another repository is another defect"
        );
    }

    #[test]
    fn passing_noise_does_not_enter_the_signature() {
        let s = sig(
            "test a ... ok\ntest result: ok. 12 passed; 0 failed; 0 ignored\n\
             test b ... FAILED\n",
        );
        assert_eq!(s.lines, vec!["test b ... FAILED".to_owned()]);
        assert_eq!(s.label, "test b ... FAILED");
    }

    #[test]
    fn output_without_a_failure_word_falls_back_to_its_tail_then_the_exit_code() {
        assert_eq!(
            sig("one\ntwo\nthree\nfour\n").lines,
            ["four", "three", "two"]
        );
        assert_eq!(sig("").lines, ["exit code 101"]);
        assert_eq!(signature("", None).lines, ["exit code none"]);
    }

    #[test]
    fn random_names_and_hashes_are_normalised_away() {
        let a = normalize("error: C:\\Temp\\.tmpAbC123 at deadbeef01 in 12ms");
        let b = normalize("error: C:\\Temp\\.tmpZzZ999 at 0123abcd99 in 7ms");
        assert_eq!(a, b);
    }

    #[test]
    fn a_task_per_failure_is_filed_once_and_a_finished_one_does_not_cover() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        let state = RunState::new(
            Path::new("/repo").to_path_buf(),
            "main".into(),
            "abc1234".into(),
            "make a note".into(),
            crate::config::Config::default(),
        );
        let s = sig("test web::tests::layout ... FAILED\n");
        let flake = VerifyFlake {
            round: Some(1),
            context: "round 1".into(),
            head: "abcdef1234567890".into(),
            command: "cargo test".into(),
            first_failure: "test web::tests::layout ... FAILED".into(),
            fingerprint: fingerprint(&state.repo, &s),
            at: jiff::Timestamp::now(),
            task: None,
            filed: false,
            filing_error: None,
        };
        let (first, created) = file(&state, &flake, &s.label, &queue).unwrap();
        assert!(created);
        let (again, created) = file(&state, &flake, &s.label, &queue).unwrap();
        assert_eq!((again.as_str(), created), (first.as_str(), false));
        assert_eq!(queue.list().len(), 1);
        let task = queue.get(&first).unwrap();
        assert!(task.instruction.contains(&marker(&flake.fingerprint)));
        assert!(task.instruction.contains("not the symptom"));
        assert!(matches!(&task.source, Source::Agent { node, .. } if node == NODE));

        queue
            .modify(&first, |t| {
                t.status = TaskStatus::Done;
                true
            })
            .unwrap();
        let (next, created) = file(&state, &flake, &s.label, &queue).unwrap();
        assert!(created, "a recurrence after the task was done is new work");
        assert_ne!(next, first);
        assert_eq!(crate::queue::short(&next), crate::queue::short(&first));
    }
}
