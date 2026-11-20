//! Read-only classification of textual stops, including diagnostic-only records.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Stop, StopKind};
use crate::run::RunState;

/// Original legacy evidence, retained in the authorization rather than invented
/// sequencer checkpoints. The digest also binds the currently approved base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// SHA-256 of the normalized source and approved base.
    pub digest: String,
    /// Original diagnostic, repair records and terminal event with owner identity.
    pub source: serde_json::Value,
}

/// Saved evidence classification, independent of filesystem and Git reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// 整合する構造化停止が保存されている。
    Recorded,
    /// 旧形式の診断と修復履歴から停止を導出できる。
    LegacyTextual,
    /// 通常のベース移動予算の枯渇。
    MovementExhausted,
    /// 停止を安全に証明する証拠が不足している。
    Insufficient,
}

/// Read-only assessment used by inspection and grant admission alike.
pub struct Assessment {
    /// 保存された証拠の分類。
    pub class: Class,
    /// 保存済み、または証拠から導出した停止。
    pub stop: Option<Stop>,
    /// 承認対象となる旧形式の証拠。
    pub legacy: Option<Evidence>,
    /// 証拠不足や矛盾の具体的な理由。
    pub missing: Option<String>,
}

impl Assessment {
    fn textual(self) -> Result<(Stop, Option<Evidence>)> {
        match self.class {
            Class::Recorded | Class::LegacyTextual => {
                Ok((self.stop.context("missing textual stop")?, self.legacy))
            }
            Class::MovementExhausted => anyhow::bail!(
                "ordinary movement exhaustion; use task control recover-base, not base-conflict repair"
            ),
            Class::Insufficient => anyhow::bail!("{}", self.missing.unwrap_or_default()),
        }
    }

    /// Stable labels for the operator-facing JSON report.
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "classification": match self.class {
                Class::Recorded => "modern-textual-conflict",
                Class::LegacyTextual => "legacy-textual-conflict",
                Class::MovementExhausted => "movement-exhaustion",
                Class::Insufficient => "insufficient-evidence",
            },
            "stop": self.stop,
            "legacy": self.legacy,
            "missing_evidence": self.missing,
        })
    }
}

/// Match both historical and current movement diagnostics exactly. Leftover
/// legacy repair records do not turn this diagnostic into a textual conflict.
pub fn classify(state: &RunState, head: &str, tip: &str) -> Assessment {
    if let (Some(sync), Some(winner)) = (&state.base_sync, state.winner()) {
        let exhausted = sync.attempts >= crate::graph::BASE_SYNC_ROUNDS
            && [("rebase(s)", "rebasing"), ("synchronization(s)", "synchronizing")]
                .iter().any(|(noun, verb)| sync.conflict.as_deref() == Some(format!(
                    "{} moved {} commit(s) ahead of {} after {} {}; {} again would only race it",
                    state.base_branch, sync.behind, winner.branch, crate::graph::BASE_SYNC_ROUNDS, noun, verb
                ).as_str()));
        let consistent = state.base_sync_recovery.stop.as_ref().is_none_or(|s| {
            s.kind == StopKind::Exhausted
                && s.head == head
                && s.tip == sync.tip
                && Some(&s.detail) == sync.conflict.as_ref()
                && s.attempts == sync.attempts
        });
        if exhausted && consistent {
            return Assessment {
                class: Class::MovementExhausted,
                stop: state.base_sync_recovery.stop.clone(),
                legacy: None,
                missing: None,
            };
        }
    }
    match derive_textual(state, head, tip) {
        Ok((stop, legacy)) => Assessment {
            class: if legacy.is_some() {
                Class::LegacyTextual
            } else {
                Class::Recorded
            },
            stop: Some(stop),
            legacy,
            missing: None,
        },
        Err(error) => Assessment {
            class: Class::Insufficient,
            stop: None,
            legacy: None,
            missing: Some(error.to_string()),
        },
    }
}

pub(super) fn textual_stop(
    state: &RunState,
    head: &str,
    tip: &str,
) -> Result<(Stop, Option<Evidence>)> {
    classify(state, head, tip).textual()
}

fn derive_textual(state: &RunState, head: &str, tip: &str) -> Result<(Stop, Option<Evidence>)> {
    let sync = state
        .base_sync
        .as_ref()
        .context("missing base synchronization evidence")?;
    let detail = sync
        .conflict
        .as_ref()
        .context("missing stopped synchronization diagnostic")?;
    let old: Vec<_> = state
        .rebase_fixes
        .iter()
        .filter(|r| r.stop.is_none())
        .collect();
    // Completed legacy rounds remain in the append-only history. Prefer proof
    // of the current structured stop; unrelated old rounds cannot downgrade it.
    let structured = state.base_sync_recovery.stop.as_ref().is_some_and(|stop| {
        state
            .base_sync_recovery
            .operations
            .iter()
            .any(|op| op.head == stop.head && op.tip == stop.tip)
            || state
                .rebase_fixes
                .iter()
                .any(|r| r.stop.is_some() && r.from.as_ref() == Some(&stop.head))
    });
    if old.is_empty() || structured {
        let stop = state.base_sync_recovery.stop.as_ref().context(
            "missing structured textual-stop evidence; use task inspect-base-stop <task> --run <run>; movement recovery cannot authorize conflict repair",
        )?;
        ensure!(
            stop.kind == StopKind::Conflict,
            "stop is {:?}, not a textual conflict; movement exhaustion uses task control recover-base",
            stop.kind
        );
        ensure!(
            structured,
            "missing structured operation or sequencer evidence; an empty repair list does not prove a modern textual stop"
        );
        ensure!(
            stop.head == head
                && stop.tip == sync.tip
                && &stop.detail == detail
                && stop.attempts == sync.attempts,
            "structured textual stop disagrees with retained HEAD/base/diagnostic/attempts"
        );
        return Ok((stop.clone(), None));
    }
    let winner = state.winner().context("missing retained winner")?;
    let cap = state.config.graph.review_rounds;
    ensure!(
        cap > 0 && old.len() == cap && old.len() == state.rebase_fixes.len(),
        "legacy evidence ambiguous: expected exactly {cap} legacy repair records, with no mixed sequencer records"
    );
    let paths = &old[0].paths;
    ensure!(
        !paths.is_empty()
            && paths.len() <= 8
            && paths.iter().all(|p| {
                !p.is_empty()
                    && !p.contains([',', '\n', '\r'])
                    && !std::path::Path::new(p).is_absolute()
                    && std::path::Path::new(p)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
            })
            && paths
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == paths.len(),
        "legacy evidence ambiguous: missing, truncated or unsafe conflict paths"
    );
    ensure!(
        old.iter().all(|r| r.from.as_deref() == Some(head)
            && !r.finished
            && r.paths == *paths
            && r.index_outside.is_none()
            && r.launcher.is_none()
            && r.infra.is_none()
            && r.outage.is_none()
            && r.error.is_none()),
        "legacy evidence ambiguous: repair from/paths/completion or launcher evidence disagree"
    );
    let prefix = format!(
        "{} conflicts with {}/{} and did not rebase: conflict not resolved after {cap} of {cap} fixer round(s) (the rounds are spent); remaining conflicted path(s): {}",
        winner.branch,
        state.config.merge.remote,
        state.base_branch,
        paths.join(", ")
    );
    ensure!(
        detail == &prefix
            || detail
                .strip_prefix(&prefix)
                .is_some_and(|s| s.starts_with("; git said: ")),
        "legacy textual diagnostic does not match retained branch, repair count and paths; this is missing textual evidence, not movement exhaustion"
    );
    let terminal: Vec<_> = state
        .events
        .iter()
        .filter(|e| e.node == "land" && &e.message == detail)
        .collect();
    ensure!(
        terminal.len() == 1,
        "legacy evidence requires one matching terminal land event (found {})",
        terminal.len()
    );
    ensure!(
        sync.attempts > 0 && sync.behind > 0,
        "legacy evidence missing stopped base reservation"
    );
    let stop = Stop {
        kind: StopKind::Conflict,
        head: head.into(),
        tip: sync.tip.clone(),
        attempts: sync.attempts,
        detail: detail.clone(),
        at: terminal[0].at,
        unverified_sync: false,
    };
    if let Some(saved) = &state.base_sync_recovery.stop {
        ensure!(
            saved == &stop,
            "legacy evidence disagrees with subsequently recorded stop; inspect retained records before authorizing"
        );
    }
    let source = serde_json::json!({"version": 1, "task": state.origin.as_ref().and_then(|o| o.task.as_ref()),
        "run": state.id, "branch": winner.branch, "base_sync": sync, "stop": stop, "repairs": state.rebase_fixes,
        "terminal": terminal[0], "approved_base": tip});
    let digest = Sha256::digest(serde_json::to_vec(&source)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((stop, Some(Evidence { digest, source })))
}

pub(crate) fn recovery_facts(
    queue: &crate::queue::Queue,
    home: &std::path::Path,
    task: &crate::queue::Task,
    state: &RunState,
) -> serde_json::Value {
    use serde_json::json;
    let resume = crate::resume::inspect_queued(home, queue, task, state);
    let movement = crate::graph::Runner::inspect_base_recovery(home, state, task);
    let scratch = home
        .join("runs")
        .join(&state.id)
        .join("base-sync")
        .try_exists();
    let stop = &state.base_sync_recovery.stop;
    let unavailable = |error: anyhow::Error| {
        let status = if error.is::<crate::resume::Unconfirmed>() || error.is::<std::io::Error>() {
            "unknown"
        } else {
            "refused"
        };
        json!({"status":status, "reason":format!("{error:#}")})
    };
    json!({
        "resume": match resume {
            Ok(()) => json!({"status": "locally_eligible"}),
            Err(error) => unavailable(error),
        },
        "recover_base": match movement {
            Ok(head) => json!({"status":"locally_eligible", "head":head, "requires_fresh_fetch_at_execution": true}),
            Err(error) => unavailable(error),
        },
        "base_stop": state.base_sync.as_ref().map(|sync| json!({
            "kind": stop.as_ref().map(|s| &s.kind),
            "recorded_head": stop.as_ref().map(|s| &s.head),
            "detail": sync.conflict, "recorded_tip": sync.tip,
            "used": state.base_episode_used(), "limit": crate::graph::BASE_SYNC_ROUNDS,
            "lifetime_attempts": sync.attempts, "scratch_present": scratch.ok(),
            "pending_authorization": state.base_recovery_pending(),
            "pending_closure": state.base_sync_closure_pending(),
            "pending_conflict_repair": super::conflict_pending(task, state),
        })),
    })
}

/// Inspect saved evidence and present Git state without acquiring writable
/// maintenance locks, fetching, changing a checkpoint or queueing a driver.
pub async fn inspect(
    queue: &crate::queue::Queue,
    home: &std::path::Path,
    task: &str,
    run: &str,
) -> Result<serde_json::Value> {
    let task = queue.get(&queue.resolve_id(task)?)?;
    let state = RunState::load_under(run, home)?;
    ensure!(
        task.execution_runs().last() == Some(&state.id)
            && state.origin.as_ref().and_then(|o| o.task.as_deref()) == Some(task.id.as_str()),
        "inspection refused: task/run ownership disagrees"
    );
    let recovery = recovery_facts(queue, home, &task, &state);
    let mut result = serde_json::json!({"task": task.id, "run": state.id, "recovery": recovery,
        "scratch_present": recovery.pointer("/base_stop/scratch_present"),
        "next_step": "Retain all records and commits. Inspect the missing evidence; do not edit JSON, reset budgets or start another run."});
    let head = match crate::graph::Runner::retained_branch_head(&state) {
        Ok(head) => head,
        Err(error) => {
            result["missing_evidence"] = format!("{error:#}").into();
            result["classification"] = "insufficient-evidence".into();
            result["verification"] = crate::chat_notify::run_verification(&state, None);
            return Ok(result);
        }
    };
    result["head"] = serde_json::json!(head);
    result["verification"] = crate::chat_notify::run_verification(&state, Some(&head));
    if !state.base_sync_blocked() {
        result["classification"] = "no-base-stop".into();
        result["next_step"] =
            "Read the current resume eligibility and verification; no base recovery is applicable."
                .into();
        return Ok(result);
    }
    let tip = crate::git::rev_parse(
        &state.repo,
        &format!(
            "refs/remotes/{}/{}",
            state.config.merge.remote, state.base_branch
        ),
    )
    .await;
    if let Err(error) = &tip {
        result["tracking_ref_unavailable"] = format!("{error:#}").into();
    }
    // A missing tracking ref does not erase saved stop classification. It does
    // prevent offering a textual repair whose target must match that ref.
    let observed_tip = tip.as_deref().unwrap_or_default();
    result["base"] = tip.as_deref().ok().into();
    result["base_source"] = "local tracking ref; not a fresh fetch".into();
    let assessment = classify(&state, &head, observed_tip);
    result
        .as_object_mut()
        .unwrap()
        .extend(assessment.report().as_object().unwrap().clone());
    if assessment.class == Class::MovementExhausted {
        if recovery["recover_base"]["status"] == "locally_eligible" {
            result["next_step"] = format!(
                "Use task control recover-base {} --request <unique-id> --head {head}; execution must fetch and revalidate",
                state.id
            ).into();
        } else {
            result["admission_blocker"] = recovery["recover_base"]["reason"].clone();
        }
        return Ok(result);
    }
    if let Ok((stop, evidence)) = assessment.textual() {
        let tip = tip.context("textual repair needs a readable local base tracking ref");
        let checks = async {
                let tip = tip.as_ref().map_err(|error| anyhow::anyhow!("{error:#}"))?;
                ensure!(!state.passes.is_empty(), "missing stopped pass checkpoint; inspect retained run provenance before recovery");
                super::controls(home, &task, &state, super::Kind::BaseConflict)?;
                super::owner_effects_settled(home, &task, &state)?;
                super::unique_owner(queue, &task, &state)?;
                ensure!(task.status != crate::queue::TaskStatus::Running, "task driver is running");
                let claimed = crate::daemon::is_working_on_other(home, &state.id, jiff::Timestamp::now())?;
                ensure!(state.liveness_under(home, claimed) == crate::run::Liveness::Dead,
                    "driver is live or unknown; wait for supported driver cleanup before granting recovery");
                super::head_and_public(&state, &head).await?;
                ensure!(
                    crate::git::is_ancestor(&state.repo, &stop.tip, tip).await,
                    "current base does not descend from stopped base"
                );
                Ok::<_, anyhow::Error>(())
            }
            .await;
        if let Err(error) = checks {
            result["admission_blocker"] = error.to_string().into();
            result["next_step"] = "Resolve the reported ownership, hold or Git-state blocker through its supported owner control, then inspect again. Inspection grants no authorization.".into();
        } else {
            let digest = evidence.as_ref().map_or(String::new(), |e| {
                format!(" --evidence-digest {}", e.digest)
            });
            let tip = tip.expect("checked tracking ref");
            result["next_step"] = format!("magi task grant-repair {} --run {} --kind base-conflict --rounds 1 --head {head} --tip {tip} --expect-pass {}{digest} --request <unique-id> --reason <reason> --instruction <approved-resolution>", task.id, state.id, state.passes.last().map_or(0, |p| p.index)).into();
        }
    }
    Ok(result)
}
