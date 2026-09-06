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
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::chat;
use crate::config::{AgentSpec, Config};
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

    let language = config.graph.language.clone();
    // Read + reason, no write: the same shape of work `[graph] timeout_judge`
    // already budgets for judges, so a stage-specific timeout nobody asked
    // for would be one more number to tune for no benefit.
    let timeout = Duration::from_secs(config.graph.timeout_judge.max(1));
    let artifacts = dir.join(format!("{id}.advisors"));
    let seed = crate::rng::entropy();

    let advice = gather(
        &seats,
        &requirements,
        &GatherCtx {
            repo,
            artifacts: &artifacts,
            run: id,
            language: &language,
            timeout,
            seed,
        },
    )
    .await;

    // Written before the checks below can bail: a total failure must still
    // leave the raw attempts on disk, or "nobody produced a proposal" is a
    // claim the operator has no way to check.
    let advice_path = dir.join(format!("{id}.advisors.json"));
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
        &config.agents,
        config.roles.planner.as_deref(),
        &plan::installed,
    )
    .context("resolving the planner seat for design synthesis")?;
    let mut seat = SeatState::new("plan-synthesis", &planner.id, seed);
    let synth_prompt = prompt::synthesize(&requirements, &proposals, &language);
    let out = agent::invoke(
        &planner,
        &mut seat,
        &Invocation {
            cwd: repo,
            prompt: &synth_prompt,
            timeout,
            allow_write: false,
            sessions: false,
            artifacts: &artifacts,
            stem: "synthesis",
            run: id,
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

    std::fs::write(draft, &synthesized).with_context(|| format!("write {}", draft.display()))?;

    Ok(advice)
}

/// The parts of [`run`]'s setup every advisor seat needs, bundled so
/// [`gather`] takes one borrow instead of a parameter per field - the same
/// reason [`crate::graph`]'s wave takes a `WaveCtx`.
struct GatherCtx<'a> {
    repo: &'a Path,
    artifacts: &'a Path,
    run: &'a str,
    language: &'a str,
    timeout: Duration,
    seed: u64,
}

/// Ask every seat for a design proposal, in parallel, headless and read-only.
///
/// Failures are per-seat, not fatal to the wave: a seat that crashes or
/// answers unparsably still produces an [`AdvisorRecord`], so one bad seat
/// does not cost the operator the other two.
async fn gather(seats: &[AgentSpec], requirements: &str, ctx: &GatherCtx<'_>) -> Advice {
    let n = seats.len();
    let mut set = tokio::task::JoinSet::new();
    for (i, spec) in seats.iter().cloned().enumerate() {
        let repo = ctx.repo.to_owned();
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
                    cwd: &repo,
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

    #[tokio::test]
    async fn gather_records_every_seat_including_one_that_fails() {
        let seats = vec![
            command("sage-a", &proposal_json("do X")),
            command("sage-b", "not json at all"),
        ];
        let dir = tempfile::tempdir().unwrap();
        let advice = gather(
            &seats,
            "the requirements",
            &GatherCtx {
                repo: dir.path(),
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

    #[tokio::test]
    async fn run_writes_the_raw_records_and_overwrites_the_draft_with_the_synthesis() {
        let tmp = tempfile::tempdir().unwrap();
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

        let advice = run(&cfg, tmp.path(), &draft, &dir, "20260906-000000-ab12")
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
    }

    #[tokio::test]
    async fn run_leaves_the_draft_untouched_when_no_advisor_produces_a_proposal() {
        let tmp = tempfile::tempdir().unwrap();
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

        let err = run(&cfg, tmp.path(), &draft, &dir, "20260906-000000-cd34")
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

        let err = run(&cfg, tmp.path(), &draft, &dir, "20260906-000000-ef56")
            .await
            .expect_err("a synthesis with no task block must fail the stage");
        let msg = err.to_string();
        assert!(msg.contains(&draft.display().to_string()), "{msg}");
        assert_eq!(std::fs::read_to_string(&draft).unwrap(), original);
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
