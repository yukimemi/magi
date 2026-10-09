//! `examples/demo_seed.rs` — state for the README's demo gif.
//!
//! `tools/demo/seed.mjs` runs this against a scratch `MAGI_HOME` so that runs,
//! questions and the queue are written through the same types the server reads
//! back. Hand-writing that JSON from node would keep working until
//! `run::SCHEMA` moved and then record an empty "Nothing has run yet" screen;
//! here a changed field is a compile error, and `cargo test --all-targets`
//! builds this file.
//!
//! ```text
//! MAGI_HOME=<scratch> demo_seed <stage> <repo>
//! ```
//!
//! Stages are cumulative states of ONE run (its id is kept in
//! `<home>/demo-run-id`), re-run in order by the recorder:
//!
//! - `inflight`: three candidates (A/B/C, no agent names), the judges still out.
//! - `reviewed`: ranking, tally, a review round with a fixed finding, a clean one.
//! - `question`: an agent question with choices, waiting for the owner.
//! - `approval`: an open pull request and the merge-approval card with its panel.
//! - `merged`: the pull request merged. No real forge or land loop runs in the
//!   demo, so this stage is the one piece of state that is seeded rather than
//!   caused by the tap that precedes it.
//!
//! Every date is relative to now, so the take never shows a stale day. The
//! agent fields are empty: the UI shows the blind labels only.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use magi::ask::Questions;
use magi::config::Config;
use magi::land::{self, Blocking, Checks, PrLifecycle, PrState};
use magi::queue::{Queue, Source, Task, TaskStatus};
use magi::run::{
    Candidate, Judgement, LandApproval, MergeOutcome, Origin, PrRecord, ReviewRecord, ReviewRound,
    RunState, RunStatus, StartedBy, Tally,
};
use magi::verdict::{Finding, ReviewVote, Severity};

const INSTRUCTION: &str = "add retry with backoff to the uploader";
const SUBJECT: &str = "feat(uploader): retry failed uploads with exponential backoff";
const PR_NUMBER: u64 = 42;
const PR_URL: &str = "https://github.com/example/uploader/pull/42";

const NUMSTAT: &str = "38\t4\tsrc/uploader.rs\n21\t0\tsrc/backoff.rs\n17\t0\ttests/retry.rs\n";

const DIFF: &str = "\
diff --git a/src/backoff.rs b/src/backoff.rs
new file mode 100644
--- /dev/null
+++ b/src/backoff.rs
@@ -0,0 +1,9 @@
+use std::time::Duration;
+
+/// Delay before attempt `n` (0-based): 200ms doubling, capped at 10s, with jitter.
+pub fn delay(n: u32, jitter: f64) -> Duration {
+    let base = 200u64.saturating_mul(1u64 << n.min(16));
+    let capped = base.min(10_000) as f64;
+    Duration::from_millis((capped * (0.5 + jitter / 2.0)) as u64)
+}
diff --git a/src/uploader.rs b/src/uploader.rs
--- a/src/uploader.rs
+++ b/src/uploader.rs
@@ -21,7 +21,14 @@ impl Uploader {
-        self.client.put(url, body).await
+        let mut attempt = 0;
+        loop {
+            match self.client.put(url, body.clone()).await {
+                Err(e) if e.is_transient() && attempt < MAX_ATTEMPTS => {
+                    tokio::time::sleep(backoff::delay(attempt, rand::random())).await;
+                    attempt += 1;
+                }
+                other => return other,
+            }
+        }
";

fn ago(minutes: i64) -> Timestamp {
    Timestamp::now() - SignedDuration::from_mins(minutes)
}

fn candidate(label: char, summary: &str, stat: &str, files: usize, secs: u64) -> Candidate {
    Candidate {
        index: (label as usize) - ('A' as usize),
        label,
        // Blind: the demo shows labels, never an agent.
        agent: String::new(),
        branch: format!("magi/demo/{label}"),
        worktree: PathBuf::new(),
        summary: summary.to_owned(),
        stat: stat.to_owned(),
        files,
        commits: 1,
        empty: false,
        failed: None,
        verified_noop: None,
        duration_ms: secs * 1000,
        folded: false,
    }
}

fn judgement(judge: usize, ranking: [char; 3], why: [&str; 3]) -> Judgement {
    Judgement {
        judge,
        seat: format!("judge-{judge}"),
        agent: String::new(),
        reasons: ranking
            .iter()
            .zip(why)
            .map(|(l, w)| (l.to_string(), w.to_owned()))
            .collect::<BTreeMap<_, _>>(),
        ranking: ranking.to_vec(),
        confidence: Some(8),
        order: vec![0, 1, 2],
        failed: None,
        duration_ms: 41_000,
    }
}

fn finding(id: &str, severity: Severity, file: &str, line: u32, title: &str) -> Finding {
    Finding {
        id: id.to_owned(),
        severity,
        file: Some(file.to_owned()),
        line: Some(line),
        title: title.to_owned(),
        detail: "A permanent error is retried as if it were transient.".to_owned(),
    }
}

fn review(
    reviewer: usize,
    summary: &str,
    findings: Vec<Finding>,
    vote: ReviewVote,
) -> ReviewRecord {
    ReviewRecord {
        reviewer,
        agent: String::new(),
        summary: summary.to_owned(),
        findings,
        vote: Some(vote),
        failed: None,
        duration_ms: 52_000,
        attempts: 0,
    }
}

/// The run as it stands at `stage`; everything an earlier stage wrote is
/// rebuilt from scratch, so any stage can be applied to a fresh home.
fn build(id: &str, repo: &std::path::Path, stage: &str) -> RunState {
    let mut run = RunState::new(
        repo.to_path_buf(),
        "main".to_owned(),
        "0000000".to_owned(),
        INSTRUCTION.to_owned(),
        Config::default(),
    );
    run.id = id.to_owned();
    run.created_at = ago(24);
    run.status = RunStatus::Judging;
    run.candidates = vec![
        candidate(
            'A',
            "Retry loop inside `upload`, fixed 1s delay, three attempts.",
            " src/uploader.rs | 19 +++++++++++--\n 1 file changed",
            1,
            214,
        ),
        candidate(
            'B',
            "Exponential backoff with jitter in its own module, transient errors only.",
            " src/backoff.rs  | 21 +++++++\n src/uploader.rs | 38 +++++++++-\n tests/retry.rs  | 17 +++++\n 3 files changed",
            3,
            268,
        ),
        candidate(
            'C',
            "Wraps the client in a generic retry middleware with a config knob.",
            " src/middleware.rs | 64 ++++++++++++\n src/uploader.rs   | 12 ++-\n 2 files changed",
            2,
            301,
        ),
    ];
    run.event("implement", "3 candidates ready");
    if stage == "inflight" {
        return run;
    }

    run.judgements = vec![
        judgement(
            1,
            ['B', 'C', 'A'],
            [
                "Backoff is isolated and tested; only transient errors retry.",
                "Sound, but the middleware is more surface than the task asks for.",
                "A fixed delay hammers a struggling server.",
            ],
        ),
        judgement(
            2,
            ['B', 'A', 'C'],
            [
                "Smallest correct design with a real test.",
                "Simple, but no jitter and no test.",
                "Over-engineered for one call site.",
            ],
        ),
        judgement(
            3,
            ['B', 'C', 'A'],
            [
                "Jitter and a cap: what the task meant by backoff.",
                "Reasonable, heavy.",
                "Fixed delay, untested.",
            ],
        ),
    ];
    run.tally = Some(Tally {
        first_choice: BTreeMap::from([('A', 0), ('B', 3), ('C', 0)]),
        borda: BTreeMap::from([('A', 1), ('B', 6), ('C', 2)]),
        winner: 'B',
        rankings: 3,
        unanimous_initial: true,
        deliberated: false,
        changed_votes: 0,
        unanimous_final: true,
        tie_break: None,
        judges: 3,
        present: 3,
        quorum: 2,
        met_quorum: true,
        uncontested: None,
    });
    let f1 = finding(
        "R1-1-1",
        Severity::Major,
        "src/uploader.rs",
        27,
        "Permanent errors are retried",
    );
    run.reviews = vec![
        ReviewRound {
            round: 1,
            head: "b3c91e0".to_owned(),
            verified_head: Some("b3c91e0".to_owned()),
            verified_at: Some(ago(9)),
            reviews: vec![
                review(
                    1,
                    "Backoff is right, but a 4xx must not be retried.",
                    vec![f1],
                    ReviewVote::ApproveWithFindings,
                ),
                review(2, "Reads well.", Vec::new(), ReviewVote::Approve),
            ],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(magi::run::FixRecord {
                agent: String::new(),
                addressed: vec!["R1-1-1".to_owned()],
                rejected: Vec::new(),
                notes: "Retry only when the error reports itself transient.".to_owned(),
                committed: true,
                failed: None,
                duration_ms: 63_000,
                continuation: None,
            }),
            blocking: 1,
            answered: 2,
            expected: 2,
            clean: false,
            progressed: true,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: Some(ReviewVote::ApproveWithFindings),
        },
        ReviewRound {
            round: 2,
            head: "e07a4d2".to_owned(),
            verified_head: Some("e07a4d2".to_owned()),
            verified_at: Some(ago(4)),
            reviews: vec![
                review(1, "The finding is fixed.", Vec::new(), ReviewVote::Approve),
                review(2, "Nothing further.", Vec::new(), ReviewVote::Approve),
            ],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 2,
            expected: 2,
            clean: true,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: Some(ReviewVote::Approve),
        },
    ];
    run.status = RunStatus::Reviewing;
    run.event("review", "round 2 clean: 2 of 2 approve");
    if matches!(stage, "reviewed" | "question") {
        return run;
    }

    run.status = RunStatus::Landing;
    run.pr = Some(PrRecord {
        url: PR_URL.to_owned(),
        number: PR_NUMBER,
        state: "open".to_owned(),
        checks: "green".to_owned(),
        round: 0,
        rounds: 3,
        red_at_merge: Vec::new(),
    });
    run.event("land", format!("opened pull request #{PR_NUMBER}"));
    if stage == "approval" {
        return run;
    }

    // `merged`: seeded, see the module docs.
    run.status = RunStatus::Merged;
    if let Some(pr) = run.pr.as_mut() {
        pr.state = "merged".to_owned();
    }
    run.merge = Some(MergeOutcome {
        mode: magi::config::MergeMode::Pr,
        ok: true,
        detail: PR_URL.to_owned(),
        empty: false,
    });
    run.event("land", format!("merged pull request #{PR_NUMBER}"));
    run
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let stage = args.next().context("usage: demo_seed <stage> <repo>")?;
    let repo = PathBuf::from(args.next().context("usage: demo_seed <stage> <repo>")?);
    if !["inflight", "reviewed", "question", "approval", "merged"].contains(&stage.as_str()) {
        bail!("unknown stage {stage:?}");
    }
    // Refuse to touch a home that was not named: `run::home()` would fall back
    // to the operator's real one.
    let Some(home) = std::env::var_os("MAGI_HOME").map(PathBuf::from) else {
        bail!("MAGI_HOME is not set; refusing to use the default home");
    };
    std::fs::create_dir_all(&home)?;

    let id_file = home.join("demo-run-id");
    let id = match std::fs::read_to_string(&id_file) {
        Ok(s) => s.trim().to_owned(),
        Err(_) => {
            let id = format!("{}-d3a0", ago(24).strftime("%Y%m%d-%H%M%S"));
            std::fs::write(&id_file, &id)?;
            id
        }
    };

    let mut run = build(&id, &repo, &stage);

    // The task the chat filed becomes the run's task: the loop is not running
    // in the demo, so this is the one place the "picked up" step is written.
    let queue = Queue::at(home.join("queue"));
    let mut task = queue
        .list()
        .into_iter()
        .find(|t| t.instruction.contains(INSTRUCTION))
        .unwrap_or_else(|| {
            Task::new(
                INSTRUCTION.to_owned(),
                INSTRUCTION.to_owned(),
                repo.clone(),
                Source::Human,
            )
        });
    task.runs = vec![id.clone()];
    task.attempts = 1;
    task.status = if stage == "merged" {
        TaskStatus::Done
    } else {
        TaskStatus::Running
    };
    queue.put(&mut task)?;
    run.origin = Some(Origin {
        by: StartedBy::Queue {
            task: task.id.clone(),
        },
        task: Some(task.id.clone()),
    });

    let questions = Questions::at(home.join("questions"));
    if matches!(stage.as_str(), "question" | "approval" | "merged") {
        let have = questions
            .list()
            .into_iter()
            .any(|q| q.node == "implement" && q.run == id);
        if !have {
            let mut q = magi::ask::Question::new(
                id.clone(),
                "implement".to_owned(),
                "impl-B".to_owned(),
                "Postgres or SQLite for the cache?".to_owned(),
                "The uploader keeps a small table of in-flight uploads. SQLite needs no server \
                 and fits a single host; Postgres survives several workers."
                    .to_owned(),
                vec!["SQLite".to_owned(), "Postgres".to_owned()],
            );
            q.asked_at = ago(6);
            questions.put(&mut q)?;
        }
    }
    if matches!(stage.as_str(), "approval" | "merged") {
        let have = questions
            .list()
            .into_iter()
            .find(|q| q.node == land::APPROVAL_NODE && q.run == id);
        if let Some(q) = have {
            run.land_approval = Some(LandApproval {
                question: q.id,
                head: "e07a4d2".to_owned(),
            });
        } else {
            let pr = PrState {
                url: PR_URL.to_owned(),
                number: PR_NUMBER,
                state: PrLifecycle::Open,
                checks: Checks::Green,
                failing: Vec::new(),
                review_comments: Vec::new(),
                blocking: Blocking::No,
            };
            let commits = vec![
                "feat(uploader): add backoff module".to_owned(),
                "fix(uploader): retry transient errors only".to_owned(),
            ];
            let html = land::approval_panel(&run, &pr, NUMSTAT, DIFF, &commits, SUBJECT);
            let mut q = magi::ask::Question::new(
                id.clone(),
                land::APPROVAL_NODE.to_owned(),
                "land".to_owned(),
                format!("merge pull request #{PR_NUMBER}: {SUBJECT}"),
                format!(
                    "{PR_URL} is green and ready to squash into `main` as `{SUBJECT}`. \
                     The panel holds the diffstat, the patch and the commits being squashed."
                ),
                vec![land::APPROVE.to_owned(), land::HOLD.to_owned()],
            );
            questions.put_panel(&mut q, &html, &[])?;
            q.asked_at = ago(1);
            questions.put(&mut q)?;
            run.land_approval = Some(LandApproval {
                question: q.id.clone(),
                head: "e07a4d2".to_owned(),
            });
        }
    }
    run.save_under(&home)?;
    println!("{id}");
    Ok(())
}
