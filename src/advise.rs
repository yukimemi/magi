//! The headless design-deliberation stage between `magi plan`'s interview and
//! the task file it files.
//!
//! [`crate::graph`]'s judge / vote / deliberate machinery only runs when more
//! than one candidate survives - and `[graph] candidates` defaults to 1 now
//! (see [`crate::config::Graph::candidates`]'s doc for the cost numbers
//! behind that default), so on an ordinary task none of that machinery ever
//! fires any more. Diversity did not stop paying for itself; it moved. A
//! design sketch is a few paragraphs an agent can write without touching the
//! repository, where a full implementation is a hundred-plus-turn tool loop
//! that re-reads the codebase on every turn - so three sketches, gathered
//! once between the interview and the file, cost a fraction of a third
//! implementation and buy back the same disagreement the judges used to
//! surface, on every task rather than only the ones run with `--candidates`.
//!
//! Three independent, read-only advisors ([`gather`]) each propose one
//! design. The planner seat (`[roles] planner`) then reads all three and
//! [`prompt::synthesize`]s them into the task file's `## Context` and
//! `## Change`, naming which advisor's idea it kept where - it is told, in so
//! many words, not to pick a winner. Nothing here talks to the operator: the
//! interview already did that, and turning this stage into three more
//! conversations would be exactly the cost this module exists to avoid.
//!
//! # Disposability narrows the blast radius, it does not forbid writing
//!
//! `allow_write: false` alone is not a guarantee: opencode has no read-only
//! mode at all, and Claude's `--disallowed-tools` stops its edit tools but not
//! a `rm` or a redirect run through its Bash tool. That is the existing,
//! accepted risk model for every read-only seat in this codebase - judge and
//! reviewer seats in [`crate::graph`] carry exactly the same weak guarantee,
//! and get away with it because their `cwd` is already a worktree the run
//! treats as disposable. This stage runs before any run exists, so it was
//! pointed at the operator's own checkout until [`checkout_worktrees`] gave
//! every advisor seat, and the planner's synthesis, a `git worktree add
//! --detach` checkout at `HEAD` of their own, thrown away when [`run`]
//! returns - the same protection judges and reviewers already had, closing
//! the one gap unique to this stage rather than inventing a stronger
//! guarantee nothing else here provides.
//!
//! What this buys: a relative-path write from a seat that ignores its
//! instructions lands in that seat's own disposable checkout, not in the
//! operator's repository and not in another seat's. What it does not buy: an
//! absolute-path write, or an edit to the shared `.git` metadata a linked
//! worktree does not copy (its own config extension aside), can still reach
//! outside the checkout - the same as it always could for a judge or a
//! reviewer. Closing that would mean sandboxing the process itself (a
//! container, a chroot, an OS-level read-only mount), which no seat of any
//! kind in this codebase has today; adding one is a different, much larger
//! change than a design-deliberation stage, not something this module can
//! give an advisor seat on its own.
//!
//! # The draft survives every failure short of success
//!
//! [`run`] never writes to `draft` until it holds a complete, synthesized
//! replacement. Every early return - an advisor roster that produced nothing
//! usable, a planner seat that crashed or answered with no fenced `task`
//! block - leaves the interview's own draft exactly as the leader wrote it,
//! and the error names its path, the same contract [`crate::plan::vet`]
//! keeps for a validation failure. The raw advisor records are written to
//! disk unconditionally, before that check even runs, so a total failure
//! still leaves something for the operator to read.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::chat;
use crate::config::{AgentSpec, Config};
use crate::git;
use crate::plan;
use crate::prompt;
use crate::verdict::{self, Proposal};

/// One advisor seat's outcome, kept even on failure so a synthesis that only
/// had two of three proposals to work with is not a mystery later - see
/// [`run`]'s doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorRecord {
    /// Seat name, e.g. `advisor-1`.
    pub seat: String,
    /// Agent id occupying the seat.
    pub agent: String,
    /// The proposal, when the seat produced a usable one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
    /// Why there is no proposal, when there is not one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall-clock duration.
    pub duration_ms: u64,
}

/// The whole deliberation: one record per advisor seat, written to
/// `<id>.advisors.json` next to the draft so the operator can read every
/// seat's reasoning - including a seat that failed - not only whichever parts
/// synthesis kept.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Advice {
    /// One record per advisor seat asked.
    pub records: Vec<AdvisorRecord>,
}

impl Advice {
    /// The seats that produced a usable proposal, in seat order.
    pub fn proposals(&self) -> Vec<(&str, &Proposal)> {
        self.records
            .iter()
            .filter_map(|r| r.proposal.as_ref().map(|p| (r.seat.as_str(), p)))
            .collect()
    }
}

/// Run the design-deliberation stage: gather independent proposals, write the
/// raw record, synthesize them under the planner seat, and overwrite `draft`
/// with the result.
///
/// `dir` is the drafts directory the raw record and the seats' artifacts are
/// written under; `id` names this interview's draft, so its siblings
/// (`<id>.advisors.json`, `<id>.advisors/`) sit next to `<id>.md` the same way
/// `<id>.briefing.md` already does.
pub async fn run(
    config: &Config,
    repo: &Path,
    draft: &Path,
    dir: &Path,
    id: &str,
) -> Result<Advice> {
    let requirements = std::fs::read_to_string(draft).with_context(|| {
        format!(
            "no task file at {} - the leader was asked to write one there",
            draft.display()
        )
    })?;

    let seats = config.advisors().context("resolving advisor seats")?;
    if seats.is_empty() {
        bail!(
            "`[graph] advisors` is 0, so there is nobody to deliberate with; \
             the interview draft is unchanged at {0} - file it as-is with \
             `magi task add --file {0}`, or set `[graph] advisors` above 0 \
             and re-run `magi plan`.",
            draft.display()
        );
    }

    let worktrees = checkout_worktrees(repo, dir, id, seats.len())
        .await
        .with_context(|| {
            format!(
                "could not prepare a disposable checkout for the advisor \
                 seats; the interview draft is unchanged at {d} - file it \
                 as-is with `magi task add --file {d}`, or retry `magi plan`.",
                d = draft.display(),
            )
        })?;

    // Split out so the worktrees are removed on every path out of here,
    // success or failure - Rust has no `try`/`finally` to hang this off of.
    let outcome = deliberate(
        &requirements,
        &seats,
        &worktrees,
        &DeliberationCtx {
            config,
            draft,
            dir,
            id,
            language: &config.graph.language,
            // Read + reason, no write: the same shape of work `[graph]
            // timeout_judge` already budgets for judges, so a stage-specific
            // timeout nobody asked for would be one more number to tune for
            // no benefit.
            timeout: Duration::from_secs(config.graph.timeout_judge.max(1)),
            seed: crate::rng::entropy(),
        },
    )
    .await;

    remove_worktrees(repo, &worktrees).await;

    outcome
}

/// Disposable, detached worktrees at `HEAD`, one per advisor seat - see the
/// module doc's "Disposability narrows the blast radius" section for why a
/// seat needs one of these rather than the operator's own checkout, and for
/// what it does and does not protect against.
///
/// Sequential, not parallel: `git worktree add` takes a lock on the
/// repository's own `.git` metadata, and setup is a one-time cost paid once
/// per `magi plan` invocation, not on the hot path a parallel seat wave
/// exists to keep cheap.
async fn checkout_worktrees(repo: &Path, dir: &Path, id: &str, n: usize) -> Result<Vec<PathBuf>> {
    let root = dir.join(format!("{id}.repo"));
    let mut paths = Vec::with_capacity(n);
    for i in 0..n {
        let wt = root.join(format!("advisor-{}", i + 1));
        if let Err(e) = git::worktree_add_detached(repo, &wt, "HEAD").await {
            // Partial setup must not leak the worktrees it did manage to
            // register before the failure that stopped it.
            remove_worktrees(repo, &paths).await;
            return Err(e);
        }
        paths.push(wt);
    }
    Ok(paths)
}

/// Best-effort teardown. A worktree `magi plan` fails to remove costs the
/// operator disk, not correctness - the advisor stage already answered or
/// already failed by the time this runs - so a removal error is logged and
/// moved past rather than turned into a second error on top of whatever
/// [`run`] is already returning.
async fn remove_worktrees(repo: &Path, worktrees: &[PathBuf]) {
    for wt in worktrees {
        if let Err(e) = git::worktree_remove(repo, wt).await {
            tracing::warn!(
                "could not remove disposable advisor worktree {}: {e:#}",
                wt.display()
            );
        }
    }
    // Only succeeds once every child above is gone; harmless otherwise.
    if let Some(root) = worktrees.first().and_then(|w| w.parent()) {
        let _ = std::fs::remove_dir(root);
    }
}

/// Everything [`deliberate`] needs once a disposable checkout exists per
/// seat, bundled so the function takes one borrow instead of a parameter per
/// field - the same reason [`crate::graph`]'s wave takes a `WaveCtx`.
struct DeliberationCtx<'a> {
    config: &'a Config,
    draft: &'a Path,
    dir: &'a Path,
    id: &'a str,
    language: &'a str,
    timeout: Duration,
    seed: u64,
}

/// The body of [`run`]: gather, record, synthesize, validate, write. Split out
/// only so [`run`] can guarantee `worktrees` are removed on every exit from
/// this, not so it can be called independently of a checkout existing.
async fn deliberate(
    requirements: &str,
    seats: &[AgentSpec],
    worktrees: &[PathBuf],
    ctx: &DeliberationCtx<'_>,
) -> Result<Advice> {
    let draft = ctx.draft;
    let artifacts = ctx.dir.join(format!("{}.advisors", ctx.id));

    let advice = gather(
        seats,
        requirements,
        worktrees,
        &GatherCtx {
            artifacts: &artifacts,
            run: ctx.id,
            language: ctx.language,
            timeout: ctx.timeout,
            seed: ctx.seed,
        },
    )
    .await;

    // Written before the checks below can bail: a total failure must still
    // leave the raw attempts on disk, or "nobody produced a proposal" is a
    // claim the operator has no way to check.
    let advice_path = ctx.dir.join(format!("{}.advisors.json", ctx.id));
    std::fs::write(
        &advice_path,
        serde_json::to_string_pretty(&advice).context("serialize the advisor records")?,
    )
    .with_context(|| format!("write {}", advice_path.display()))?;

    let proposals = advice.proposals();
    if proposals.is_empty() {
        bail!(
            "none of {n} advisor seat(s) produced a usable design proposal \
             (see {record}); the interview draft is unchanged at {d} - file \
             it as-is with `magi task add --file {d}`, or retry `magi plan`.",
            n = seats.len(),
            record = advice_path.display(),
            d = draft.display(),
        );
    }

    let planner = plan::pick(
        &ctx.config.agents,
        ctx.config.roles.planner.as_deref(),
        &plan::installed,
    )
    .context("resolving the planner seat for design synthesis")?;
    let mut seat = SeatState::new("plan-synthesis", &planner.id, ctx.seed);
    let synth_prompt = prompt::synthesize(requirements, &proposals, ctx.language);
    let out = agent::invoke(
        &planner,
        &mut seat,
        &Invocation {
            // The first advisor's disposable checkout, reused: every advisor
            // task has already finished by this point (`gather` awaited them
            // all), so there is nothing left to race with, and a fourth
            // checkout just for synthesis would buy nothing this one does
            // not already give it - a read-only view of the repository that
            // is not the operator's own.
            cwd: &worktrees[0],
            prompt: &synth_prompt,
            timeout: ctx.timeout,
            allow_write: false,
            sessions: false,
            artifacts: &artifacts,
            stem: "synthesis",
            run: ctx.id,
            node: "plan-advise",
            cache_dir: None,
        },
    )
    .await
    .with_context(|| {
        format!(
            "the planner seat could not synthesize the design proposals; the \
             interview draft is unchanged at {0} - file it as-is with `magi \
             task add --file {0}`, or retry `magi plan`.",
            draft.display()
        )
    })?;

    if !out.usable() {
        bail!(
            "the planner seat produced nothing usable while synthesizing the \
             design proposals; the interview draft is unchanged at {0} - file \
             it as-is with `magi task add --file {0}`, or retry `magi plan`.",
            draft.display()
        );
    }

    let synthesized = chat::extract_draft(&out.text).with_context(|| {
        format!(
            "the planner seat's reply had no fenced ```task block; the \
             interview draft is unchanged at {0} - file it as-is with `magi \
             task add --file {0}`, or retry `magi plan`.",
            draft.display()
        )
    })?;

    // Checked before a single byte reaches `draft`: `extract_draft` accepts an
    // unclosed fence as "whatever came before end of stream", so a synthesis
    // that stopped mid-sentence - a truncated reply, not a system timeout -
    // would otherwise overwrite a perfectly good interview draft with
    // something `vet` rejects one call later, at which point the operator's
    // requirements are already gone. The same shape `vet` itself uses: length
    // alone warns rather than refuses, everything else must hold.
    if let Err(problems) = plan::review_draft(&synthesized) {
        let hard: Vec<&String> = problems
            .iter()
            .filter(|p| p.as_str() != plan::SHORT_DRAFT)
            .collect();
        if !hard.is_empty() {
            let list = hard
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "the planner seat's synthesis is not a usable task file:\n{list}\n\n\
                 the interview draft is unchanged at {d} - file it as-is with \
                 `magi task add --file {d}`, or retry `magi plan`.",
                d = draft.display(),
            );
        }
    }

    std::fs::write(draft, &synthesized).with_context(|| format!("write {}", draft.display()))?;

    Ok(advice)
}

/// The parts of [`deliberate`]'s setup every advisor seat needs, bundled so
/// [`gather`] takes one borrow instead of a parameter per field - the same
/// reason [`crate::graph`]'s wave takes a `WaveCtx`.
struct GatherCtx<'a> {
    artifacts: &'a Path,
    run: &'a str,
    language: &'a str,
    timeout: Duration,
    seed: u64,
}

/// Ask every seat for a design proposal, in parallel, headless and read-only,
/// each in its own disposable worktree (`worktrees[i]` for `seats[i]`).
///
/// Failures are per-seat, not fatal to the wave: a seat that crashes or
/// answers unparsably still produces an [`AdvisorRecord`], so one bad seat
/// does not cost the operator the other two.
async fn gather(
    seats: &[AgentSpec],
    requirements: &str,
    worktrees: &[PathBuf],
    ctx: &GatherCtx<'_>,
) -> Advice {
    let n = seats.len();
    let mut set = tokio::task::JoinSet::new();
    for (i, spec) in seats.iter().cloned().enumerate() {
        let cwd = worktrees[i].clone();
        let requirements = requirements.to_owned();
        let artifacts = ctx.artifacts.to_owned();
        let run = ctx.run.to_owned();
        let language = ctx.language.to_owned();
        let timeout = ctx.timeout;
        let seed = ctx.seed;
        let key = format!("advisor-{}", i + 1);
        set.spawn(async move {
            let mut seat = SeatState::new(&key, &spec.id, seed ^ (i as u64 + 1));
            let prompt = prompt::advisor(&requirements, i + 1, n, &language);
            let started = Instant::now();
            let outcome = agent::invoke(
                &spec,
                &mut seat,
                &Invocation {
                    cwd: &cwd,
                    prompt: &prompt,
                    timeout,
                    allow_write: false,
                    sessions: false,
                    artifacts: &artifacts,
                    stem: &key,
                    run: &run,
                    node: "plan-advise",
                    cache_dir: None,
                },
            )
            .await;
            to_record(key, spec.id, started.elapsed(), outcome)
        });
    }
    let mut records = Vec::with_capacity(n);
    while let Some(res) = set.join_next().await {
        records.push(match res {
            Ok(rec) => rec,
            Err(e) => AdvisorRecord {
                seat: "?".to_owned(),
                agent: "?".to_owned(),
                proposal: None,
                error: Some(format!("advisor task panicked: {e}")),
                duration_ms: 0,
            },
        });
    }
    // Stable seat order for a readable record: a `JoinSet` completes in
    // whichever order the seats actually answered, not seat 1, 2, 3.
    records.sort_by(|a, b| a.seat.cmp(&b.seat));
    Advice { records }
}

fn to_record(
    seat: String,
    agent_id: String,
    elapsed: Duration,
    outcome: Result<agent::AgentOutput>,
) -> AdvisorRecord {
    match outcome {
        Ok(out) if out.usable() => {
            match verdict::extract_json::<Proposal>(&out.text)
                .and_then(|p| p.validate().map(|()| p))
            {
                Ok(proposal) => AdvisorRecord {
                    seat,
                    agent: agent_id,
                    proposal: Some(proposal),
                    error: None,
                    duration_ms: out.duration_ms,
                },
                Err(e) => AdvisorRecord {
                    seat,
                    agent: agent_id,
                    proposal: None,
                    error: Some(e.to_string()),
                    duration_ms: out.duration_ms,
                },
            }
        }
        Ok(out) => AdvisorRecord {
            seat,
            agent: agent_id,
            proposal: None,
            error: Some(if out.timed_out {
                "timed out".to_owned()
            } else {
                format!("exit {:?}: {}", out.exit_code, out.text.trim())
            }),
            duration_ms: out.duration_ms,
        },
        Err(e) => AdvisorRecord {
            seat,
            agent: agent_id,
            proposal: None,
            error: Some(e.to_string()),
            duration_ms: elapsed.as_millis() as u64,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentKind, Graph, Roles};

    /// A `kind = "command"` agent that discards its prompt and prints `output`
    /// verbatim: `cat` drains stdin so `invoke`'s writer never blocks, and the
    /// heredoc's quoted delimiter keeps `sh` from expanding anything inside
    /// `output` - the same trick a JSON block full of `{`/`}` and a `task`
    /// fence full of markdown both need.
    fn command(id: &str, output: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("cat >/dev/null && cat <<'EOF'\n{output}\nEOF"),
            ],
            extra_args: Vec::new(),
            env: Default::default(),
            prompt_delivery: None,
        }
    }

    fn proposal_json(approach: &str) -> String {
        format!(
            "```json\n{{\"approach\":\"{approach}\",\"key_tradeoff\":\"t\",\
             \"risks\":[\"r\"],\"touches\":[\"src/a.rs\"],\
             \"why_not_naive\":\"w\"}}\n```"
        )
    }

    fn good_draft() -> String {
        "# Rework the config loader\n\
         \n\
         ## Context\n\
         \n\
         placeholder context.\n\
         \n\
         ## Change\n\
         \n\
         placeholder change.\n\
         \n\
         ## Constraints\n\
         \n\
         No new dependencies.\n\
         \n\
         ## Completion criteria\n\
         \n\
         - [ ] it works\n\
         \n\
         ## Out of scope\n\
         \n\
         nothing\n"
            .to_owned()
    }

    fn synthesized_task_block() -> String {
        format!(
            "```task\n{}```",
            good_draft().replace("placeholder", "synthesized")
        )
    }

    /// A real git repository with one commit, so `checkout_worktrees` has a
    /// `HEAD` to detach from. A plain temp directory is enough for the tests
    /// that fail before that point (no draft, zero advisors); only the ones
    /// that reach the disposable checkout need this.
    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir_all(dir).unwrap();
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "magi test"]);
        run(&["config", "user.email", "magi@example.com"]);
        std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "init"]);
    }

    #[tokio::test]
    async fn gather_records_every_seat_including_one_that_fails() {
        let seats = vec![
            command("sage-a", &proposal_json("do X")),
            command("sage-b", "not json at all"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let worktrees = vec![dir.path().join("wt-1"), dir.path().join("wt-2")];
        for wt in &worktrees {
            std::fs::create_dir_all(wt).unwrap();
        }
        let advice = gather(
            &seats,
            "the requirements",
            &worktrees,
            &GatherCtx {
                artifacts: &dir.path().join("artifacts"),
                run: "test-run",
                language: "en",
                timeout: Duration::from_secs(30),
                seed: 7,
            },
        )
        .await;

        assert_eq!(advice.records.len(), 2);
        assert_eq!(advice.records[0].seat, "advisor-1");
        assert_eq!(advice.records[1].seat, "advisor-2");
        let ok = advice.records[0]
            .proposal
            .as_ref()
            .expect("advisor-1 parses");
        assert_eq!(ok.approach, "do X");
        assert!(advice.records[1].proposal.is_none());
        assert!(advice.records[1].error.is_some());
    }

    fn config(agents: Vec<AgentSpec>, advisors: usize) -> Config {
        Config {
            agents,
            roles: Roles {
                advisors: vec!["sage-a".to_owned(), "sage-b".to_owned()],
                planner: Some("planner".to_owned()),
                ..Roles::default()
            },
            graph: Graph {
                advisors,
                ..Graph::default()
            },
            ..Config::default()
        }
    }

    /// This does not prove a seat *cannot* write - `allow_write: false` is
    /// not enforced by every CLI kind (opencode has no read-only mode at
    /// all), and nothing here forbids an absolute-path write either. What it
    /// proves is the gap that was unique to this stage: a relative-path
    /// write from a seat that ignores its instructions used to land in the
    /// operator's own repository - the one directory this stage must never
    /// touch - and now lands in that seat's own disposable checkout instead.
    #[tokio::test]
    async fn a_relative_path_write_from_an_advisor_lands_in_its_worktree_not_the_operators_repository()
     {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        let draft = dir.join("20260906-000000-kl12.md");
        std::fs::write(&draft, good_draft()).unwrap();

        // Ignores its own read-only instruction and writes a file anyway -
        // standing in for a CLI kind (or a Bash tool) `allow_write: false`
        // does not actually stop.
        let writer = AgentSpec {
            id: "sage-a".to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!(
                    "cat >/dev/null && touch leaked-by-advisor.txt && cat <<'EOF'\n{}\nEOF",
                    proposal_json("do X")
                ),
            ],
            extra_args: Vec::new(),
            env: Default::default(),
            prompt_delivery: None,
        };

        let cfg = config(
            vec![
                writer,
                command("sage-b", &proposal_json("do Y")),
                command("planner", &synthesized_task_block()),
            ],
            2,
        );

        run(&cfg, &repo, &draft, &dir, "20260906-000000-kl12")
            .await
            .expect("deliberation still succeeds even though a seat wrote something");

        assert!(
            !repo.join("leaked-by-advisor.txt").exists(),
            "an advisor's write must land in its disposable worktree, never in the operator's repository"
        );
    }

    #[tokio::test]
    async fn run_writes_the_raw_records_and_overwrites_the_draft_with_the_synthesis() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        let draft = dir.join("20260906-000000-ab12.md");
        std::fs::write(&draft, good_draft()).unwrap();

        let cfg = config(
            vec![
                command("sage-a", &proposal_json("do X")),
                command("sage-b", &proposal_json("do Y")),
                command("planner", &synthesized_task_block()),
            ],
            2,
        );

        let advice = run(&cfg, &repo, &draft, &dir, "20260906-000000-ab12")
            .await
            .expect("deliberation succeeds");
        assert_eq!(advice.proposals().len(), 2);

        let advice_path = dir.join("20260906-000000-ab12.advisors.json");
        let raw = std::fs::read_to_string(&advice_path).expect("raw record on disk");
        let reread: Advice = serde_json::from_str(&raw).expect("parses back");
        assert_eq!(reread.records.len(), 2);

        let final_draft = std::fs::read_to_string(&draft).unwrap();
        assert!(
            final_draft.contains("synthesized context"),
            "the draft must be overwritten with the synthesis: {final_draft}"
        );
        assert!(final_draft.contains("## Completion criteria"));

        // The disposable checkouts must not survive a successful run - a
        // seat's worktree left behind would be exactly the write surface this
        // whole isolation exists to avoid leaving around.
        assert!(
            !dir.join("20260906-000000-ab12.repo").exists(),
            "advisor worktrees must be cleaned up after the run"
        );
    }

    #[tokio::test]
    async fn run_leaves_the_draft_untouched_when_no_advisor_produces_a_proposal() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        let draft = dir.join("20260906-000000-cd34.md");
        let original = good_draft();
        std::fs::write(&draft, &original).unwrap();

        let cfg = config(
            vec![
                command("sage-a", "garbage"),
                command("sage-b", "also garbage"),
                command("planner", &synthesized_task_block()),
            ],
            2,
        );

        let err = run(&cfg, &repo, &draft, &dir, "20260906-000000-cd34")
            .await
            .expect_err("no proposal must fail the stage");
        let msg = err.to_string();
        assert!(msg.contains(&draft.display().to_string()), "{msg}");
        assert!(msg.contains("magi task add --file"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(&draft).unwrap(),
            original,
            "the interview draft must survive a total advisor failure"
        );
        // The raw attempt is still on disk, garbage and all - the operator can
        // read why every seat failed even though nothing was usable.
        assert!(dir.join("20260906-000000-cd34.advisors.json").is_file());
    }

    #[tokio::test]
    async fn run_leaves_the_draft_untouched_when_the_planner_replies_with_no_task_block() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        let draft = dir.join("20260906-000000-ef56.md");
        let original = good_draft();
        std::fs::write(&draft, &original).unwrap();

        let cfg = config(
            vec![
                command("sage-a", &proposal_json("do X")),
                command("sage-b", &proposal_json("do Y")),
                command("planner", "sure, here is my answer with no fence"),
            ],
            2,
        );

        let err = run(&cfg, &repo, &draft, &dir, "20260906-000000-ef56")
            .await
            .expect_err("a synthesis with no task block must fail the stage");
        let msg = err.to_string();
        assert!(msg.contains(&draft.display().to_string()), "{msg}");
        assert_eq!(std::fs::read_to_string(&draft).unwrap(), original);
    }

    /// Reported: `chat::extract_draft` accepts an unclosed fence as whatever
    /// came before end of stream, so a planner reply that stops mid-sentence
    /// - not a timeout, `out.usable()` is still true - used to overwrite a
    /// perfectly good interview draft with a stub `vet` then rejected one
    /// call later, by which point the original requirements were gone.
    #[tokio::test]
    async fn run_leaves_the_draft_untouched_when_the_synthesis_fence_never_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        init_repo(&repo);
        let dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        let draft = dir.join("20260906-000000-ij90.md");
        let original = good_draft();
        std::fs::write(&draft, &original).unwrap();

        let cfg = config(
            vec![
                command("sage-a", &proposal_json("do X")),
                command("sage-b", &proposal_json("do Y")),
                command("planner", "```task\n# incomplete"),
            ],
            2,
        );

        let err = run(&cfg, &repo, &draft, &dir, "20260906-000000-ij90")
            .await
            .expect_err("an incomplete synthesis must not become the task file");
        let msg = err.to_string();
        assert!(msg.contains(&draft.display().to_string()), "{msg}");
        assert!(msg.contains("not a usable task file"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(&draft).unwrap(),
            original,
            "the interview draft must survive an incomplete synthesis"
        );
    }

    #[tokio::test]
    async fn run_reports_a_missing_draft_against_the_path_the_leader_was_given() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        let draft = dir.join("never-written.md");

        let cfg = config(vec![command("sage-a", &proposal_json("x"))], 1);
        let msg = run(&cfg, tmp.path(), &draft, &dir, "never-written")
            .await
            .expect_err("nothing to deliberate over")
            .to_string();
        assert!(msg.contains(&draft.display().to_string()), "{msg}");
    }

    #[tokio::test]
    async fn zero_advisors_is_an_error_that_still_names_the_draft() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("drafts");
        std::fs::create_dir_all(&dir).unwrap();
        let draft = dir.join("20260906-000000-gh78.md");
        std::fs::write(&draft, good_draft()).unwrap();

        let cfg = config(vec![command("sage-a", &proposal_json("x"))], 0);
        let msg = run(&cfg, tmp.path(), &draft, &dir, "20260906-000000-gh78")
            .await
            .expect_err("nobody to deliberate with")
            .to_string();
        assert!(msg.contains("advisors` is 0"), "{msg}");
        assert!(msg.contains(&draft.display().to_string()), "{msg}");
    }
}
