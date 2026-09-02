//! Shared fixture for the end-to-end graph tests.
//!
//! The mock agent is a POSIX shell script driven by `kind = "command"`. It
//! dispatches on the prompt magi wrote for it (`MAGI_PROMPT_FILE`), which means
//! the tests exercise the *real* prompts: if a node's wording changes so much
//! that the phrase a reviewer keys on disappears, these tests notice.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use magi::config::{
    AgentKind, AgentSpec, Blind, Config, Graph, Merge, MergeMode, Roles, Update, UpdateMode, Verify,
};

/// `set_home` is a process-wide global, so scenes that share one test binary
/// (multiple runs, includes a `--resume`) must not run concurrently or they
/// will clobber each other's run directory. Take one of these for the life of
/// the test to serialize them. Async-aware because the guard is held across
/// the `execute` awaits.
static HOME_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Hold for the duration of a test that touches a magi run.
pub async fn home_lock() -> tokio::sync::MutexGuard<'static, ()> {
    HOME_LOCK.lock().await
}

/// Judge behaviour for a scenario.
pub enum Judges {
    /// Every judge ranks `A` first: no deliberation.
    Unanimous,
    /// Judges rank `A`, `B`, `C` first respectively, then all vote `B`.
    Split,
}

/// A temporary repository plus a mock agent.
pub struct Fixture {
    /// Keeps the temp tree alive.
    pub tmp: tempfile::TempDir,
    /// The repository magi operates on.
    pub repo: PathBuf,
    /// Config with a three-agent roster of mocks.
    pub config: Config,
}

const MOCK: &str = r#"#!/bin/sh
# Mock agent. Dispatches on the prompt magi generated for this seat.
set -e
p="$MAGI_PROMPT_FILE"
seat="$MAGI_SEAT"

# Rate-limit simulation: a matching seat reports the same error shape a real
# claude prints on quota exhaustion, and exits non-zero. The graph must read
# this as "rate limited" — not a normal failure, not retried.
#
# Deliberately NOT gated on which prompt arrived. A quota is a property of the
# account, not of the question: an exhausted seat answers every prompt the same
# way, including a retry nudge. Gating it on the judging prompt let the nudge
# fall through to the implementation branch below, so a seat that had just been
# rate limited answered a retry with a SUMMARY block claiming it created
# note.txt — which looks like success, and hid the very retry this forbids.
if [ -n "$MOCK_QUOTA_SEAT" ] && { case ",$MOCK_QUOTA_SEAT," in *",$seat,"*) true ;; *) false ;; esac; }; then
  printf '{"is_error":true,"terminal_reason":"api_error","result":"You'\''ve hit your session limit","session_id":"quota-test"}\n'
  exit 1
fi

# Ordinary-failure simulation: a matching judge seat produces no usable output
# and exits non-zero — distinctly NOT the rate-limit shape above, so the graph
# must treat it as a plain failure (retried the configured number of times),
# meanwhile still collapsing the quorum if enough seats drop.
if [ -n "$MOCK_FAILED_SEAT" ] && { case ",$MOCK_FAILED_SEAT," in *",$seat,"*) true ;; *) false ;; esac; } && grep -q "independent judges" "$p"; then
  echo 'not a ranking at all'
  exit 1
fi

if grep -q "Final vote" "$p"; then
  printf '```json\n{"vote":"%s","reason":"mock final vote"}\n```\n' "$MOCK_VOTE"
  exit 0
fi

if grep -q "deliberation round" "$p"; then
  printf '## POSITION\nThe mock argues for %s and cites nothing.\n\n' "$MOCK_VOTE"
  printf '```json\n{"tentative":"%s"}\n```\n' "$MOCK_VOTE"
  exit 0
fi

if grep -q "independent judges" "$p"; then
  n="${seat#judge-}"
  if [ "$MOCK_JUDGES" = "split" ]; then
    case "$n" in
      1) r='["A","B","C"]' ;;
      2) r='["B","C","A"]' ;;
      *) r='["C","A","B"]' ;;
    esac
  else
    r='["A","B","C"]'
  fi
  printf '```json\n{"ranking":%s,"reasons":{"A":"mock"},"confidence":4}\n```\n' "$r"
  exit 0
fi

if grep -q "reviewers of" "$p"; then
  if [ -n "$MOCK_FINDING" ] && [ ! -f fixed.txt ]; then
    printf '{"summary":"mock review","findings":[{"severity":"blocker","file":"note.txt","line":1,"title":"needs a fixed marker","detail":"create fixed.txt"}]}\n'
  else
    printf '{"summary":"mock review: clean","findings":[]}\n'
  fi
  exit 0
fi

if grep -q "Your patch was reviewed" "$p"; then
  echo "fixed" > fixed.txt
  git add -A >/dev/null 2>&1
  git commit -q -m "address review findings" >/dev/null 2>&1
  id=$(grep -o 'R[0-9]*-[0-9]*-[0-9]*' "$p" | head -1)
  printf '{"addressed":["%s"],"rejected":[],"notes":"created fixed.txt"}\n' "$id"
  exit 0
fi

# Anything else is the implementation node.
echo "content from $seat" > note.txt
git add -A >/dev/null 2>&1
git commit -q -m "add note from $seat" >/dev/null 2>&1
printf '## SUMMARY\n- created note.txt\n- no risks\n'
"#;

fn run_git(repo: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Build a repository, a mock agent, and a config wired to both.
///
/// `require_fix` makes the reviewers raise one blocking finding on the first
/// round, so the review + fix loop is actually exercised.
pub fn fixture(judges: Judges, require_fix: bool) -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    run_git(&repo, &["init", "-b", "main"]);
    run_git(&repo, &["config", "user.name", "magi test"]);
    run_git(&repo, &["config", "user.email", "magi@example.com"]);
    std::fs::write(repo.join("README.md"), "# fixture\n").unwrap();
    run_git(&repo, &["add", "-A"]);
    run_git(&repo, &["commit", "-m", "init"]);

    let script = tmp.path().join("mock-agent.sh");
    std::fs::write(&script, MOCK).unwrap();

    // Keep every run and worktree inside the temp tree.
    magi::run::set_home(tmp.path().join("magi-home"));

    let (judge_mode, vote) = match judges {
        Judges::Unanimous => ("unanimous", "A"),
        Judges::Split => ("split", "B"),
    };
    let mut env = BTreeMap::from([
        ("MOCK_JUDGES".to_owned(), judge_mode.to_owned()),
        ("MOCK_VOTE".to_owned(), vote.to_owned()),
    ]);
    if require_fix {
        env.insert("MOCK_FINDING".to_owned(), "1".to_owned());
    }

    let agent = |id: &str| AgentSpec {
        id: id.to_owned(),
        kind: AgentKind::Command,
        model: None,
        command: vec!["sh".to_owned(), script.to_string_lossy().into_owned()],
        extra_args: Vec::new(),
        env: env.clone(),
        prompt_delivery: None,
    };

    let config = Config {
        agents: vec![agent("alpha"), agent("beta"), agent("gamma")],
        roles: Roles::default(),
        graph: Graph {
            candidates: 3,
            judges: 3,
            deliberate_rounds: 1,
            reviewers: 2,
            review_rounds: 3,
            max_parallel: 3,
            language: "en".to_owned(),
            sessions: true,
            timeout_implement: 120,
            timeout_judge: 120,
            timeout_review: 120,
            timeout_fix: 120,
            retries: 1,
            worktree_root: Some(tmp.path().join("wt")),
        },
        // Pinned so the label assignment and the per-judge presentation orders
        // are the same on every run. Without it the run id supplies the seed,
        // and any assertion about *which* order a judge got is a dice roll —
        // three judges shuffling three candidates land on the same permutation
        // about once in 36 runs, which is exactly how `test (windows-latest)`
        // went red on an unrelated Renovate PR.
        blind: Blind {
            seed: Some(20_260_830),
            ..Blind::default()
        },
        verify: Verify {
            e2e: vec!["test -f note.txt".to_owned()],
            gate: vec!["test -f note.txt".to_owned()],
            shell: Some(vec!["sh".to_owned(), "-c".to_owned()]),
        },
        merge: Merge {
            mode: MergeMode::None,
            base: None,
            remote: "origin".to_owned(),
        },
        // Tests must never reach the network.
        update: Update {
            mode: UpdateMode::Off,
            interval: None,
        },
    };

    Fixture { tmp, repo, config }
}

/// Like [`fixture`], but the given judge seats hit a simulated rate limit at
/// their initial ranking. `Judges::Unanimous` keeps the surviving judges
/// agreeing, so the only variable left is how many seats remain.
pub fn fixture_with_quota(quota_seats: &[&str]) -> Fixture {
    let mut fx = fixture(Judges::Unanimous, false);
    let value = quota_seats.join(",");
    for a in &mut fx.config.agents {
        a.env.insert("MOCK_QUOTA_SEAT".to_owned(), value.clone());
    }
    fx
}

/// Like [`fixture`], but the given judge seats fail with a *plain* error (no
/// usable output) at their initial ranking — the non-quota counterpart to
/// [`fixture_with_quota`]. Enough of these collapse the quorum the same way a
/// rate limit does.
pub fn fixture_with_failure(failed_seats: &[&str]) -> Fixture {
    let mut fx = fixture(Judges::Unanimous, false);
    let value = failed_seats.join(",");
    for a in &mut fx.config.agents {
        a.env.insert("MOCK_FAILED_SEAT".to_owned(), value.clone());
    }
    fx
}
