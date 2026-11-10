//! Explicit recovery of the answer-recorded / effect-applied ordering race.
//! The grant stays immutable; this ledger never creates or refunds an attempt.
use super::*;
use crate::ask::{Question, QuestionStatus, Questions};

/// Exact retained authorization selected by the owner for reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::Args)]
pub struct Request {
    /// Owning task.
    pub task: String,
    /// Retained run.
    #[arg(long)]
    pub run: String,
    /// Full retained head.
    #[arg(long)]
    pub head: String,
    /// Retained pass index.
    #[arg(long)]
    pub expect_pass: usize,
    /// Existing grant request id; no new grant is issued.
    #[arg(long)]
    pub receipt: String,
    /// Original answered triage question.
    #[arg(long)]
    pub question: String,
    /// Exact original answer timestamp, including fractional seconds.
    #[arg(long)]
    pub answer_at: jiff::Timestamp,
    /// Idempotency key for this reconciliation.
    #[arg(long)]
    pub request: String,
    /// Explicit owner's reason.
    #[arg(long)]
    pub reason: String,
}

/// Append-only proof bound to the immutable grant and source answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// Exact owner request.
    pub request: Request,
    /// Original authorization, preserved byte for byte in the task ledger.
    pub grant: super::Receipt,
    /// Exact source response evidence, independent of message delivery.
    pub source: String,
    /// Durable application evidence from the task's resumption record.
    pub application: String,
    /// Decision produced by the original answer's application.
    pub decision: jiff::Timestamp,
    /// Unanswered generic dependency questions superseded by this exact recovery.
    pub superseded_questions: Vec<(String, String)>,
}

pub(crate) fn source(q: &Question) -> Result<String> {
    Ok(serde_json::to_string(&(
        &q.id,
        &q.run,
        &q.node,
        &q.summary,
        &q.detail,
        &q.seat,
        &q.answer_authority,
        &q.choices,
        &q.actions,
        q.status,
        q.asked_at,
        q.answered_at,
        q.accepted_answer_at,
        &q.answer,
        q.triage_generation,
        &q.routed_task,
    ))?)
}

fn prove(task: &Task, grant: &super::Receipt, q: &Question) -> Result<String> {
    let release = task
        .resumption
        .as_ref()
        .context("original decision application is missing")?;
    let previous = task.resumption_history.last();
    ensure!(
        matches!(grant.request.kind, Kind::Review | Kind::Gate)
            && task.review_branch.is_none()
            && q.run == task.id
            && q.seat == "triage"
            && q.actions.is_empty()
            && matches!(
                q.node.as_str(),
                crate::triage::NODE | crate::triage::DEPS_NODE
            )
            && q.status == QuestionStatus::Answered
            && q.answered_at.is_some_and(|at| at <= grant.at)
            && q.accepted_answer_at == q.answered_at
            && q.triage_generation == release.generation.checked_sub(1)
            && q.resolution().as_ref() == q.choices.first()
            && task.triage_applied(&q.id)
            && release.question_id.as_deref() == Some(&q.id)
            && release.source
                == if q.node == crate::triage::NODE {
                    "triage"
                } else {
                    "triage-dependencies"
                }
            && release.at > grant.at
            && task.operator_decided_at == Some(release.at)
            && release.instruction == task.instruction
            && release
                .answers_through
                .checked_sub(1)
                .and_then(|i| task.answers.get(i))
                .is_some_and(
                    |a| a.question == q.summary && Some(a.answer.clone()) == q.resolution()
                )
            && previous.map(|r| r.at) == grant.decision
            && previous.map_or(release.generation == 1, |r| r.generation + 1
                == release.generation)
            && contract(task, grant.request.kind)? == grant.contract,
        "repair receipt recovery refused: exact original answer/application or unchanged authorization cannot be proved; inspect source evidence, do not issue a duplicate grant"
    );
    if let Some(effect) = task.decision_effects.get(&q.id) {
        ensure!(
            effect == &effect_evidence(q, grant.decision, &grant.contract, release)?,
            "original decision source/application evidence changed"
        );
    }
    Ok(serde_json::to_string(&application_identity(release))?)
}

/// Durable no-op: generic resume cannot reconcile or replace repair authority.
pub(crate) fn blocked_resume(q: &Question) -> Result<String> {
    Ok(format!("repair-receipt-blocked:{}", source(q)?))
}

/// Conductor deduplication is mutable after release; it is not part of the
/// owner's authorization. Every decision timestamp and scope field is retained.
pub(crate) fn application_identity(after: &crate::queue::Resumption) -> crate::queue::Resumption {
    let mut identity = after.clone();
    identity.blocking_evidence.clear();
    identity
}

/// Captured atomically with successful triage release; it is not a timestamp-only receipt.
pub(crate) fn effect_evidence(
    q: &Question,
    before: Option<jiff::Timestamp>,
    contract: &str,
    after: &crate::queue::Resumption,
) -> Result<String> {
    Ok(serde_json::to_string(&(
        source(q)?,
        before,
        contract,
        application_identity(after),
    ))?)
}

/// Only an unchanged unanswered generic dependency question can be superseded.
/// A subsequent answer is a new decision and immediately invalidates this exemption.
pub(crate) fn superseded_question(task: &Task, q: &Question) -> bool {
    task.repair_reconciliations.iter().any(|r| {
        r.superseded_questions
            .iter()
            .any(|(id, evidence)| id == &q.id && source(q).is_ok_and(|s| &s == evidence))
    })
}

/// Consumption and ordinary resume share this check; later decisions still win.
pub(super) fn decision_matches(home: &Path, task: &Task, grant: &super::Receipt) -> Result<bool> {
    if task.operator_decided_at == grant.decision {
        return Ok(true);
    }
    for receipt in &task.repair_reconciliations {
        if &receipt.grant == grant && task.operator_decided_at == Some(receipt.decision) {
            let q = Questions::at(home.join("questions")).get(&receipt.request.question)?;
            return Ok(
                source(&q)? == receipt.source && prove(task, grant, &q)? == receipt.application
            );
        }
    }
    Ok(false)
}

/// Supported owner operation. A crash before the single task write changes
/// nothing; a retry after it returns the same receipt without queuing again.
pub async fn apply(queue: &Queue, home: &Path, mut request: Request) -> Result<Receipt> {
    ensure!(
        !request.reason.trim().is_empty() && request.reason.len() <= 4096,
        "reason must contain 1..=4096 bytes"
    );
    ensure!(
        !request.request.is_empty()
            && request.request.len() <= 128
            && request
                .request
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)),
        "invalid reconciliation request id"
    );
    request.task = queue.resolve_id(&request.task)?;
    let _claim = queue.claim(&request.task)?;
    let mut task = queue.get(&request.task)?;
    if let Some(prior) = task
        .repair_reconciliations
        .iter()
        .find(|r| r.request.request == request.request)
    {
        ensure!(
            prior.request == request,
            "reconciliation request id has different arguments"
        );
        return Ok(prior.clone());
    }
    let grant = task
        .repair_grants
        .last()
        .context("no retained repair receipt")?
        .clone();
    ensure!(
        grant.request.request == request.receipt
            && grant.request.run == request.run
            && grant.request.head == request.head
            && grant.request.expect_pass == request.expect_pass,
        "reconciliation must name the exact retained task/run/head/pass/receipt"
    );
    ensure!(
        !task.repair_reconciliations.iter().any(|r| r.grant == grant),
        "receipt already reconciled; retry the original reconciliation request id"
    );
    let _owner = crate::pr_head::owner_lock(home, &request.run)?;
    let state = RunState::load_under(&request.run, home)?;
    let questions = Questions::at(home.join("questions"));
    let mut superseded_questions = Vec::new();
    for q in questions.snapshot()? {
        if q.run == task.id
            && q.node == crate::triage::DEPS_NODE
            && q.status.open()
            && q.answer.is_none()
            && q.answered_at.is_none()
            && task
                .resumption
                .as_ref()
                .is_some_and(|r| q.asked_at > r.at && q.triage_generation == Some(r.generation))
        {
            superseded_questions.push((q.id.clone(), source(&q)?));
        }
    }
    let q = questions.get(&request.question)?;
    ensure!(
        q.answered_at == Some(request.answer_at),
        "source answer generation changed"
    );
    let application = prove(&task, &grant, &q)?;
    let receipt = Receipt {
        request: request.clone(),
        grant: grant.clone(),
        source: source(&q)?,
        application,
        decision: task
            .operator_decided_at
            .context("missing applied decision")?,
        superseded_questions,
    };
    // Stage the exemptions in memory for validation; authority and the pin are
    // published together only by the final queue write below.
    task.repair_reconciliations.push(receipt.clone());
    controls(home, &task, &state, grant.request.kind)?;
    owner_effects_settled(home, &task, &state)?;
    unique_owner(queue, &task, &state)?;
    ensure!(task.status != TaskStatus::Running, "task driver is running");
    let claimed = crate::daemon::is_working_on_other(home, &state.id, jiff::Timestamp::now())?;
    ensure!(
        state.liveness_under(home, claimed) == Liveness::Dead,
        "driver is live or unknown"
    );
    ensure!(
        task.repair_grants
            .iter()
            .filter(|g| g.request.run == state.id
                && !superseded(&task, g)
                && !state.repair_grants.iter().any(|a| a.receipt == **g))
            .count()
            == 1,
        "recovery requires exactly one retained unimported grant"
    );
    let _execution = crate::driver::maintenance(home, &state.id)?;
    let _outputs = output_leases(&state)?;
    ensure!(
        !state.repair_grants.iter().any(|r| r.receipt == grant),
        "receipt was already imported; recovery requires an unimported, unspent receipt"
    );
    evidence(&state, &grant.request)?;
    head_and_public(&state, &request.head).await?;
    task.status = TaskStatus::Queued;
    task.hold_source = None;
    task.hold_reason = None;
    task.held_at = None;
    task.resume_override = Some(crate::queue::OperatorResume {
        question_id: String::new(),
        at: jiff::Timestamp::now(),
        conductor_rehold: None,
        forced: true,
        pinned_run: Some(state.id),
    });
    queue.put(&mut task)?;
    Ok(receipt)
}

/// Diagnostic only. Actual recovery revalidates under all execution locks.
pub(crate) fn blocker(queue: &Queue, task: &Task) -> Option<String> {
    let grant = task.repair_grants.last()?;
    if task.execution_runs().last() != Some(&grant.request.run) || superseded(task, grant) {
        return None;
    }
    let home = queue.root().parent()?;
    let state = RunState::load_under(&grant.request.run, home).ok()?;
    if state.repair_grants.iter().any(|a| a.receipt == *grant)
        || decision_matches(home, task, grant).unwrap_or(false)
    {
        return None;
    }
    if task
        .repair_reconciliations
        .iter()
        .any(|r| r.grant == *grant)
    {
        return Some(format!(
            "Retained repair receipt {} was already reconciled but its authorization or source evidence changed. Keep this run and receipt; inspect the later owner decision/source evidence. Replaying recovery returns its historical receipt and cannot release a later hold. Do not use generic resume or issue another grant.",
            grant.request.request
        ));
    }
    let original = task
        .resumption
        .as_ref()
        .and_then(|r| r.question_id.as_ref())
        .and_then(|id| Questions::at(home.join("questions")).get(id).ok());
    let Some(q) = original.filter(|q| q.answered_at.is_some()) else {
        return Some(format!(
            "Retained repair receipt {} is blocked by a changed owner decision, but its original question/application evidence is missing. Keep the retained task/run/receipt and inspect its decision history; generic resume and a new grant cannot reconcile missing authority.",
            grant.request.request
        ));
    };
    let question = &q.id;
    let answer_at = q.answered_at?;
    Some(format!(
        "Retained repair receipt {} needs explicit reconciliation, not generic resume or new work. Inspect the original question {question} and its exact answered_at, then on owner authorization use: magi task control reconcile-repair-grant {} --run {} --head {} --expect-pass {} --receipt {} --question {question} --answer-at {answer_at} --request <stable-recovery-id> --reason <owner-reason>. This adds no repair budget; missing or changed evidence is refused.",
        grant.request.request,
        task.id,
        grant.request.run,
        grant.request.head,
        grant.request.expect_pass,
        grant.request.request
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_application() -> (Task, super::super::Receipt, Question) {
        let mut task = Task::new(
            "fixture".into(),
            "retain the original work".into(),
            std::path::PathBuf::from("fixture"),
            crate::queue::Source::Human,
        );
        let mut q = Question::new(
            task.id.clone(),
            crate::triage::DEPS_NODE.into(),
            "triage".into(),
            "release?".into(),
            "retained dependency".into(),
            vec!["release".into(), "hold".into()],
        );
        q.asked_at = "2026-01-01T00:00:00Z".parse().unwrap();
        q.answered_at = Some("2026-01-01T00:00:01Z".parse().unwrap());
        q.accepted_answer_at = q.answered_at;
        q.status = QuestionStatus::Answered;
        q.answer = Some(crate::ask::Answer::Choice(q.choices[0].clone()));
        q.triage_generation = Some(0);
        let grant = super::super::Receipt {
            request: super::super::Request {
                task: task.id.clone(),
                run: "20260101-000000-abcd".into(),
                kind: Kind::Review,
                rounds: 1,
                head: "a".repeat(40),
                expect_pass: 0,
                request: "original".into(),
                reason: "one repair".into(),
                tip: None,
                instruction: None,
                replace_request: None,
            },
            at: "2026-01-01T00:00:02Z".parse().unwrap(),
            decision: None,
            contract: contract(&task, Kind::Review).unwrap(),
            stop: None,
        };
        // The old worker committed the response and release together, without
        // the modern full-source receipt. This is durable application evidence,
        // unlike a triage_applied marker alone.
        task.answers.push(crate::queue::AnsweredQuestion {
            question: q.summary.clone(),
            answer: q.resolution().unwrap(),
        });
        task.release_operator("triage-dependencies", Some(&q.id));
        task.mark_triage_applied(&q.id);
        (task, grant, q)
    }

    #[test]
    fn legacy_recovery_requires_exact_application_not_a_marker_or_timestamp() {
        let (task, grant, q) = legacy_application();
        assert!(prove(&task, &grant, &q).is_ok());
        let task: Task = serde_json::from_slice(&serde_json::to_vec(&task).unwrap()).unwrap();
        let q: Question = serde_json::from_slice(&serde_json::to_vec(&q).unwrap()).unwrap();
        assert!(prove(&task, &grant, &q).is_ok(), "restart retains proof");
        let mut changed = q.clone();
        changed.answer = Some(crate::ask::Answer::Choice("hold".into()));
        assert!(prove(&task, &grant, &changed).is_err());
        let mut changed = q.clone();
        changed.triage_generation = Some(1);
        assert!(prove(&task, &grant, &changed).is_err());
        let mut changed = q.clone();
        changed.accepted_answer_at = None;
        assert!(prove(&task, &grant, &changed).is_err());
        let mut changed = q.clone();
        changed.run = "unrelated-task".into();
        assert!(prove(&task, &grant, &changed).is_err());
        let mut changed = task.clone();
        changed.resumption = None;
        assert!(prove(&changed, &grant, &q).is_err());
        let mut changed = task.clone();
        changed.answers.clear();
        assert!(prove(&changed, &grant, &q).is_err());
        let mut changed = task.clone();
        changed.hold_manual(Some("owner refuses".into()));
        assert!(prove(&changed, &grant, &q).is_err());
        let mut changed = task.clone();
        changed.release_operator("later decision", None);
        assert!(prove(&changed, &grant, &q).is_err());
        let mut changed = task.clone();
        changed.instruction.push_str(" new criteria");
        assert!(prove(&changed, &grant, &q).is_err());
        let mut changed = grant.clone();
        changed.decision = Some(grant.at);
        assert!(prove(&task, &changed, &q).is_err());
        let mut with_receipt = task.clone();
        with_receipt.decision_effects.insert(
            q.id.clone(),
            effect_evidence(
                &q,
                grant.decision,
                &grant.contract,
                task.resumption.as_ref().unwrap(),
            )
            .unwrap(),
        );
        with_receipt
            .resumption
            .as_mut()
            .unwrap()
            .blocking_evidence
            .push("conductor deduplication receipt".into());
        assert!(prove(&with_receipt, &grant, &q).is_ok());
        let mut changed_source = q.clone();
        changed_source.detail.push_str(" changed context");
        assert!(prove(&with_receipt, &grant, &changed_source).is_err());
        let mut changed = task.clone();
        changed
            .decision_effects
            .insert(q.id.clone(), "uncertain external effect".into());
        assert!(prove(&changed, &grant, &q).is_err());
    }
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::task_decision::Status;

    #[test]
    fn native_pending_uncertain_and_replayed_decisions_are_task_scoped() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut task = Task::new(
            "fixture".into(),
            "retain work".into(),
            tmp.path().into(),
            crate::queue::Source::Human,
        );
        queue.put(&mut task).unwrap();
        let state = RunState::new(
            task.repo.clone(),
            "main".into(),
            "a".repeat(40),
            task.instruction.clone(),
            crate::config::Config::default(),
        );
        let outbox = crate::task_inquiry::Outbox::at(tmp.path());
        let inquiry = outbox
            .file_for_occurrence(
                &task,
                None,
                "fixture".into(),
                "owner-effect",
                "Owner effect",
                "Inspect receipt",
            )
            .unwrap();
        assert!(owner_effects_settled(tmp.path(), &task, &state).is_ok());
        outbox
            .update_decision(&inquiry.id, |d| d.accept("exact original request"))
            .unwrap();
        for status in [Status::Queued, Status::InProgress, Status::RetryNeeded] {
            outbox
                .update_decision(&inquiry.id, |d| {
                    d.status = status;
                    Ok(true)
                })
                .unwrap();
            let restarted = crate::task_inquiry::Outbox::at(tmp.path());
            assert_eq!(
                restarted.get(&inquiry.id).unwrap().decision.unwrap().status,
                status
            );
            assert!(owner_effects_settled(tmp.path(), &task, &state).is_err());
            let mut other = task.clone();
            other.id = "unrelated".into();
            assert!(owner_effects_settled(tmp.path(), &other, &state).is_ok());
        }
        outbox
            .update_decision(&inquiry.id, |d| d.applied("exact durable effect receipt"))
            .unwrap();
        assert!(owner_effects_settled(tmp.path(), &task, &state).is_ok());
        let before = outbox.get(&inquiry.id).unwrap();
        outbox
            .update_decision(&inquiry.id, |d| d.accept("exact original request"))
            .unwrap();
        assert_eq!(outbox.get(&inquiry.id).unwrap(), before);
        assert!(owner_effects_settled(tmp.path(), &task, &state).is_ok());
    }
}
