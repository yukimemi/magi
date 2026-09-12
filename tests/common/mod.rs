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
    AgentKind, AgentSpec, Blind, Config, Graph, Merge, MergeMode, MergeStyle, Roles, Update,
    UpdateMode, Verify,
};

/// `set_home` is a process-wide global, so scenes that share one test binary
/// (multiple runs, includes a `--resume`) must not run concurrently or they
/// will clobber each other's run directory. Take one of these for the life of
/// the test to serialize them. Async-aware because the guard is held across
/// the `execute` awaits.
static HOME_LOCK: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Proof of having called [`home_lock`], for the life of the test.
///
/// Every `fixture*` constructor below demands one by reference. That is not
/// decoration: a fixture that could be built without it is exactly how a test
/// forgets to serialize against `run::set_home`'s process-wide `OnceLock` and
/// silently steals the shared home out from under whatever else the suite is
/// running — the mistake reads clean in isolation and only shows up as some
/// *other* test's failure once the binary runs its tests together. Making the
/// guard a parameter turns "forgot to lock" into a compile error instead of a
/// flake somebody else has to chase down.
pub type HomeGuard = tokio::sync::MutexGuard<'static, ()>;

/// Hold for the duration of a test that touches a magi run.
pub async fn home_lock() -> HomeGuard {
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
    /// Held for as long as the fixture is: a reference would let the guard
    /// backing it drop at the end of the constructor call instead of the end
    /// of the test, which is exactly early enough to let two fixtures in the
    /// same test binary race `run::set_home` again.
    _home: HomeGuard,
}

const MOCK: &str = r#"#!/bin/sh
# Mock agent. Dispatches on the prompt magi generated for this seat.
set -e
p="$MAGI_PROMPT_FILE"
seat="$MAGI_SEAT"

# Attribution trail. `magi task add` turns MAGI_RUN / MAGI_NODE into a task's
# source, so a run whose agents never receive them would silently attribute
# agent-filed work to a passing human. Recording them here is what lets a test
# assert the plumbing from inside a real graph run.
printf '%s %s %s\n' "$MAGI_RUN" "$MAGI_NODE" "$seat" \
  >> "$(dirname "$p")/attribution.log"

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

# Dropped-stream simulation: a matching implement seat's first reply is the
# "billed work, nothing delivered" shape a CLI leaves when it hangs up on its
# own stream mid-response (see `agent::dropped_stream`). The resumed call is
# recognisable by the words `prompt::resume_after_drop` actually sends, and
# falls through to the ordinary implementation branch at the bottom so it
# always succeeds.
if [ -n "$MOCK_DROPPED_SEAT" ] && { case ",$MOCK_DROPPED_SEAT," in *",$seat,"*) true ;; *) false ;; esac; } && ! grep -q "Your last reply never reached me" "$p"; then
  printf '{"conversation_id":"mock-convo-%s","status":"ERROR","response":"","error":"subscriber fell behind updates, stalled for 5s","usage":{"output_tokens":500}}\n' "$seat"
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

# Dropped-stream simulation, deliberation-only: a matching judge's
# *deliberation round* reply (never its initial ranking or its vote) is the
# same "billed work, nothing delivered" shape as above. Regression coverage
# for `deliberate()`'s own `AgentOutcome` handling, which must skip the turn
# rather than record the CLI's raw error JSON as the judge's position.
if [ -n "$MOCK_DROPPED_DELIBERATE_SEAT" ] && { case ",$MOCK_DROPPED_DELIBERATE_SEAT," in *",$seat,"*) true ;; *) false ;; esac; } && grep -q "deliberation round" "$p"; then
  printf '{"conversation_id":"mock-convo-%s","status":"ERROR","response":"","error":"subscriber fell behind updates, stalled for 5s","usage":{"output_tokens":500}}\n' "$seat"
  exit 1
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

if grep -q "Your revote" "$p"; then
  # Reconsideration after a split vote. A matching seat holds the same
  # `approve_with_findings` vote it cast initially; every other seat holds
  # its `approve`. Deterministic on purpose: the fixtures that exercise this
  # branch assert the round's recorded verdict, not a specific argument.
  if [ -n "$MOCK_SPLIT_REVIEW_SEAT" ] && { case ",$MOCK_SPLIT_REVIEW_SEAT," in *",$seat,"*) true ;; *) false ;; esac; }; then
    printf '{"vote":"approve_with_findings","reason":"mock reconsideration: the nit stands but is not blocking"}\n'
  elif [ -n "$MOCK_ALWAYS_FINDING" ] || { [ -n "$MOCK_FINDING" ] && [ ! -f fixed.txt ]; }; then
    printf '{"vote":"reject","reason":"mock reconsideration: the finding still holds"}\n'
  else
    printf '{"vote":"approve","reason":"mock reconsideration: nothing outstanding"}\n'
  fi
  exit 0
fi

if grep -q "reviewers of" "$p"; then
  # Silent-seat simulation: a matching review seat produces nothing usable and
  # exits non-zero, which is exactly the record a real timeout leaves — the
  # graph sees one `ReviewRecord` with `failed` set either way.
  #
  # Not `sleep`ing past a short `timeout_review` to make the kill do it: on
  # Windows there is no `execve`, so MSYS emulates `exec` by spawning a fresh
  # process and letting the shell exit. magi kills the process it spawned —
  # the shell — and the sleeper survives it, holding the inherited stdout
  # handle and a cwd inside the run's temp tree. The test then pays the whole
  # sleep at teardown rather than one timeout: measured at 302s against a 45s
  # `timeout_review`. The timeout leg itself is covered where it belongs, in
  # `agent::tests::timeout_is_reported_not_hung`.
  if [ -n "$MOCK_SILENT_SEAT" ] && { case ",$MOCK_SILENT_SEAT," in *",$seat,"*) true ;; *) false ;; esac; }; then
    echo 'not a review at all'
    exit 1
  fi
  # Split-vote simulation: a matching seat casts the lone dissenting vote —
  # fine to proceed, but with a finding — while every other seat below
  # approves clean. The finding is deliberately non-blocking, so the round
  # still gates on its own: only the vote tally disagrees.
  if [ -n "$MOCK_SPLIT_REVIEW_SEAT" ] && { case ",$MOCK_SPLIT_REVIEW_SEAT," in *",$seat,"*) true ;; *) false ;; esac; }; then
    printf '{"summary":"mock split review","vote":"approve_with_findings","findings":[{"severity":"minor","file":"note.txt","line":1,"title":"nit: consider a comment","detail":"cosmetic only"}]}\n'
    exit 0
  fi
  if [ -n "$MOCK_ALWAYS_FINDING" ] || { [ -n "$MOCK_FINDING" ] && [ ! -f fixed.txt ]; }; then
    printf '{"summary":"mock review","vote":"reject","findings":[{"severity":"blocker","file":"note.txt","line":1,"title":"needs a fixed marker","detail":"create fixed.txt"}]}\n'
  else
    printf '{"summary":"mock review: clean","vote":"approve","findings":[]}\n'
  fi
  exit 0
fi

if grep -q "Your patch was reviewed" "$p"; then
  id=$(grep -o 'R[0-9]*-[0-9]*-[0-9]*' "$p" | head -1)
  # A fixer that claims to have addressed the finding but never touches the
  # tree — the self-report `graph::Runner::review_loop` no longer trusts for
  # whether a round made progress.
  if [ -n "$MOCK_FIXER_NOOP" ]; then
    printf '{"addressed":["%s"],"rejected":[],"notes":"claims to have fixed it"}\n' "$id"
    exit 0
  fi
  if [ -n "$MOCK_FIXER_EMPTY_COMMIT" ]; then
    git commit --allow-empty -q -m "address review findings" >/dev/null 2>&1
    printf '{"addressed":["%s"],"rejected":[],"notes":"empty commit"}\n' "$id"
    exit 0
  fi
  # Appended with a nonce so a fixer invoked round after round always has a
  # real diff to commit — otherwise `git commit` on an unchanged `fixed.txt`
  # fails with nothing to commit, and the loop stops early on "the fixer
  # produced no commit" instead of actually exhausting `review_rounds`.
  echo "fixed $$" >> fixed.txt
  git add -A >/dev/null 2>&1
  git commit -q -m "address review findings" >/dev/null 2>&1
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
///
/// `home` is [`home_lock`]'s guard, taken by value and stored on the returned
/// `Fixture` so it stays held for the fixture's own lifetime rather than
/// dropping at the end of this call.
pub fn fixture(home: HomeGuard, judges: Judges, require_fix: bool) -> Fixture {
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
            // Everything else at its default. Naming every field here means a
            // new config option breaks four integration tests that do not care
            // about it, which is a cost with no matching benefit.
            ..Graph::default()
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
            style: MergeStyle::default(),
            remote: "origin".to_owned(),
            release_bump: true,
        },
        // Tests must never reach the network.
        update: Update {
            mode: UpdateMode::Off,
            interval: None,
        },
        ..Config::default()
    };

    Fixture {
        tmp,
        repo,
        config,
        _home: home,
    }
}

/// Like [`fixture`], but the given judge seats hit a simulated rate limit at
/// their initial ranking. `Judges::Unanimous` keeps the surviving judges
/// agreeing, so the only variable left is how many seats remain.
pub fn fixture_with_quota(home: HomeGuard, quota_seats: &[&str]) -> Fixture {
    let mut fx = fixture(home, Judges::Unanimous, false);
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
pub fn fixture_with_failure(home: HomeGuard, failed_seats: &[&str]) -> Fixture {
    let mut fx = fixture(home, Judges::Unanimous, false);
    let value = failed_seats.join(",");
    for a in &mut fx.config.agents {
        a.env.insert("MOCK_FAILED_SEAT".to_owned(), value.clone());
    }
    fx
}

/// Like [`fixture`], but the given implement seats' first reply is the
/// "billed work, nothing delivered" shape a CLI leaves when it hung up on its
/// own stream — the counterpart to [`fixture_with_failure`] for
/// `agent::dropped_stream`. The resumed call always succeeds, so a candidate
/// that gets resumed ends up healthy and one that does not (no session left to
/// resume) ends up looking like an ordinary failure.
pub fn fixture_with_dropped_stream(home: HomeGuard, seats: &[&str]) -> Fixture {
    let mut fx = fixture(home, Judges::Unanimous, false);
    let value = seats.join(",");
    for a in &mut fx.config.agents {
        a.env.insert("MOCK_DROPPED_SEAT".to_owned(), value.clone());
    }
    fx
}

/// A solo candidate (`graph.candidates = 1`, the shipped default) whose
/// reviewers raise a blocking finding every round, no matter what the fixer
/// does, **and** whose own e2e never passes — so the review loop exhausts
/// its budget with a real red command in hand and the run ends `Blocked`.
/// A round budget spent with *green* verification is a hand-off, not a
/// block (see `graph::Runner::stop_reviewing`); this fixture is for the
/// genuinely-blocked leg specifically. Built for reentry tests: a single
/// viable candidate is what makes `judge` skip the panel instead of asking
/// it.
pub fn fixture_always_blocked(home: HomeGuard) -> Fixture {
    let mut fx = fixture(home, Judges::Unanimous, true);
    fx.config.graph.candidates = 1;
    fx.config.verify.e2e = vec!["false".to_owned()];
    for a in &mut fx.config.agents {
        a.env
            .insert("MOCK_ALWAYS_FINDING".to_owned(), "1".to_owned());
    }
    fx
}

/// Like [`fixture_with_dropped_stream`], but the dropped shape happens on a
/// judge's *deliberation round* reply instead of an implementer's. Needs
/// `Judges::Split` — deliberation only opens once judges disagree.
pub fn fixture_with_dropped_deliberation(home: HomeGuard, seats: &[&str]) -> Fixture {
    let mut fx = fixture(home, Judges::Split, false);
    let value = seats.join(",");
    for a in &mut fx.config.agents {
        a.env
            .insert("MOCK_DROPPED_DELIBERATE_SEAT".to_owned(), value.clone());
    }
    fx
}

/// Like [`fixture`], but the given review seats never answer: they exit
/// non-zero with nothing a `Review` can be parsed out of, so the round records
/// them with `failed` set. The other seats review normally with no findings,
/// so the only reason any round is not clean is the missing seat.
///
/// That record is what a real reviewer timeout also leaves — the graph cannot
/// tell the two apart, and does not need to. Simulating the timeout itself
/// with a sleep costs the *whole sleep* on Windows rather than one
/// `timeout_review` (see the `MOCK_SILENT_SEAT` comment in the mock: 302s
/// measured against a 45s budget), and the timeout leg has its own coverage in
/// `agent::tests::timeout_is_reported_not_hung`. The classification this
/// fixture feeds is additionally pinned process-free by the
/// `graph::tests::round_is_clean` family.
pub fn fixture_with_silent_review_seat(home: HomeGuard, silent_seats: &[&str]) -> Fixture {
    let mut fx = fixture(home, Judges::Unanimous, false);
    // No re-ask: the seat is meant to be absent from the round, not absent
    // once and then absent again.
    fx.config.graph.retries = 0;
    let value = silent_seats.join(",");
    for a in &mut fx.config.agents {
        a.env.insert("MOCK_SILENT_SEAT".to_owned(), value.clone());
    }
    fx
}

/// Like [`fixture`], but the given review seat casts a lone
/// `approve_with_findings` vote (with one non-blocking finding) while every
/// other seat approves clean — a split that never produces a blocking
/// finding, so the round still gates clean and the only thing worth
/// asserting is what the vote machinery itself did: `vote_split`,
/// `reconsideration`, and `verdict` on the round record.
///
/// The mock's reconsideration branch has the split seat hold its vote and
/// every other seat hold theirs, so the round's final verdict is
/// deterministic: `approve_with_findings`, the more cautious of the two.
pub fn fixture_with_split_review_vote(home: HomeGuard, split_seat: &str) -> Fixture {
    let mut fx = fixture(home, Judges::Unanimous, false);
    for a in &mut fx.config.agents {
        a.env
            .insert("MOCK_SPLIT_REVIEW_SEAT".to_owned(), split_seat.to_owned());
    }
    fx
}

/// Reviewers that never run out of a blocking finding to raise, so the review
/// loop can be driven all the way to `rounds` without ever going clean. The
/// fixer still runs and still commits a real, distinct change every round
/// (see the mock script) — the point of this fixture is a round budget spent
/// on a change that keeps moving, never a tree that stopped moving.
pub fn fixture_that_never_clears(home: HomeGuard, rounds: usize) -> Fixture {
    let mut fx = fixture(home, Judges::Unanimous, false);
    fx.config.graph.review_rounds = rounds;
    for a in &mut fx.config.agents {
        a.env
            .insert("MOCK_ALWAYS_FINDING".to_owned(), "1".to_owned());
    }
    fx
}

/// Like [`fixture_that_never_clears`], but the fixer also never actually
/// touches the tree — it reports `addressed` while leaving the diff exactly
/// as it was. This is what a vibrating round looks like from `git`'s side,
/// as opposed to what the fixer's own report claims.
pub fn fixture_with_noop_fixer(home: HomeGuard, rounds: usize) -> Fixture {
    let mut fx = fixture_that_never_clears(home, rounds);
    for a in &mut fx.config.agents {
        a.env.insert("MOCK_FIXER_NOOP".to_owned(), "1".to_owned());
    }
    fx
}
