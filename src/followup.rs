//! Follow-up tasks for the findings a merged run left open.
//!
//! A run can hand off after its review budget is spent with findings still
//! open (and, with `land_approval` off, merge unattended). Those findings
//! live in the pull request body, which nobody reads after the merge. This
//! module files them as queue tasks instead, once the merge is confirmed.
//!
//! What is filed: every Major-or-above finding of the last review round, and
//! every finding of a seat whose final vote was reject (a vote carries no
//! per-finding reasoning, so the seat's Minor/Nit findings come along). Other
//! Minor/Nit findings are only listed, in the pull-request comment.
//!
//! Best-effort by construction, like `bump::after_merge`: [`after_merge`]
//! returns `()`, records every failure as a run event and never touches
//! `status`.

use anyhow::Result;

use crate::queue::{FollowUp, Queue, Source, Task};
use crate::run::{FollowupRecord, RunState};
use crate::verdict::{Finding, ReviewVote};

/// Node name carried by [`Source::Agent`] on a filed follow-up.
pub const NODE: &str = "followup";

/// A task of this generation (or deeper) does not file follow-ups of its own.
pub const MAX_FOLLOWUP_GENERATION: u32 = 2;

/// Two findings in one file this many lines apart are one defect.
const LINE_WINDOW: u32 = 5;

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Task ids created by this pass.
    pub filed: Vec<String>,
    /// Findings selected but not filed (cap reached), as ids.
    pub capped: Vec<String>,
    /// Minor/Nit findings of the last round that were only listed.
    pub unfiled: Vec<String>,
}

/// File follow-ups for a run that just merged, and comment on the pull
/// request. Never fails: problems become run events.
pub async fn after_merge(state: &mut RunState, pr_url: &str) {
    if !state.config.graph.file_followups {
        return;
    }
    let queue = Queue::open();
    let outcome = match file(state, pr_url, &queue) {
        Ok(o) => o,
        Err(e) => {
            state.event(NODE, format!("follow-up filing failed: {e:#}"));
            return;
        }
    };
    if outcome.filed.is_empty() && (state.followups.is_empty() || state.followup_commented) {
        return;
    }
    let body = comment_body(state, &outcome);
    match crate::bump::gh_pr_comment(&state.repo, pr_url, &body).await {
        Ok(()) => state.followup_commented = true,
        Err(e) => state.event(NODE, format!("follow-up comment not posted: {e:#}")),
    }
}

/// The deterministic half of [`after_merge`]: select, group, and file into
/// `queue`. Records the event and the run's own bookkeeping; the caller
/// saves the state.
pub fn file(state: &mut RunState, pr_url: &str, queue: &Queue) -> Result<Outcome> {
    let mut out = Outcome::default();
    let Some(round) = state.reviews.last().cloned() else {
        return Ok(out);
    };
    let rejecters: Vec<usize> = round
        .final_votes()
        .into_iter()
        .filter(|(_, _, v)| *v == ReviewVote::Reject)
        .map(|(seat, _, _)| seat)
        .collect();
    let mut chosen: Vec<Finding> = Vec::new();
    for rec in &round.reviews {
        let rejected = rejecters.contains(&rec.reviewer);
        for f in &rec.findings {
            if f.severity.blocks() || rejected {
                chosen.push(f.clone());
            } else {
                out.unfiled.push(f.id.clone());
            }
        }
    }
    if chosen.is_empty() {
        return Ok(out);
    }

    let origin_task = state.origin.as_ref().and_then(|o| o.task.clone());
    let parent_gen = match origin_task
        .as_deref()
        .and_then(|t| queue.get(t).ok())
        .map(|t| t.followup.map_or(0, |f| f.generation))
    {
        Some(g) => {
            state.followup_generation = Some(g);
            g
        }
        None => state.followup_generation.unwrap_or(0),
    };
    if parent_gen >= MAX_FOLLOWUP_GENERATION {
        out.capped = chosen.iter().map(|f| f.id.clone()).collect();
        state.event(
            NODE,
            format!(
                "generation cap reached ({parent_gen}); not filing follow-ups for open finding(s): {}",
                out.capped.join(", ")
            ),
        );
        return Ok(out);
    }

    let mut done: Vec<String> = state
        .followups
        .iter()
        .flat_map(|r| r.findings.iter().cloned())
        .collect();
    for t in queue.list() {
        if let Some(f) = t.followup
            && f.run == state.id
        {
            done.extend(f.findings);
        }
    }

    let mut failure = None;
    for group in group_findings(chosen) {
        let ids: Vec<String> = group.iter().map(|f| f.id.clone()).collect();
        if ids.iter().any(|id| done.contains(id)) {
            continue;
        }
        let mut task = build_task(state, pr_url, origin_task.clone(), &group, parent_gen + 1);
        match queue.create_new(&mut task) {
            Ok(created) => {
                if created {
                    out.filed.push(task.id.clone());
                }
                state.followups.push(FollowupRecord {
                    task: task.id,
                    findings: ids,
                });
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }
    }
    if !out.filed.is_empty() {
        state.event(
            NODE,
            format!(
                "filed {} follow-up task(s): ids {}",
                out.filed.len(),
                out.filed.join(", ")
            ),
        );
    }
    if let Some(e) = failure {
        state.event(NODE, format!("follow-up filing stopped: {e:#}"));
    }
    Ok(out)
}

/// Group findings that are the same defect: same file with lines within
/// [`LINE_WINDOW`] of each other, or, with no file, the same normalized
/// title. Deterministic, so a re-run groups identically.
fn group_findings(mut findings: Vec<Finding>) -> Vec<Vec<Finding>> {
    findings.sort_by(|a, b| (&a.file, a.line, &a.id).cmp(&(&b.file, b.line, &b.id)));
    let mut groups: Vec<Vec<Finding>> = Vec::new();
    for f in findings {
        let hit = groups
            .iter()
            .position(|g| g.iter().any(|o| same_defect(o, &f)));
        match hit {
            Some(i) => groups[i].push(f),
            None => groups.push(vec![f]),
        }
    }
    for g in &mut groups {
        g.sort_by(|a, b| a.id.cmp(&b.id));
    }
    groups
}

fn same_defect(a: &Finding, b: &Finding) -> bool {
    match (&a.file, &b.file) {
        (Some(x), Some(y)) => {
            x == y
                && match (a.line, b.line) {
                    (Some(l), Some(m)) => l.abs_diff(m) <= LINE_WINDOW,
                    (None, None) => true,
                    _ => false,
                }
        }
        (None, None) => normalize(&a.title) == normalize(&b.title),
        _ => false,
    }
}

fn normalize(title: &str) -> String {
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Deterministic id: the run id plus a hash of the group's finding ids, so
/// two passes name the same task and [`Queue::create_new`] can refuse the
/// second. Ends in a `-` segment like every id, so `queue::short` works.
fn task_id(run: &str, ids: &[String]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in ids.join("+").bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{run}-f{:05x}", h & 0xf_ffff)
}

fn location(f: &Finding) -> String {
    match (&f.file, f.line) {
        (Some(p), Some(l)) => format!("{p}:{l}"),
        (Some(p), None) => p.clone(),
        _ => "(no location)".to_owned(),
    }
}

fn build_task(
    state: &RunState,
    pr_url: &str,
    origin_task: Option<String>,
    group: &[Finding],
    generation: u32,
) -> Task {
    let ids: Vec<String> = group.iter().map(|f| f.id.clone()).collect();
    let first = &group[0];
    let title = format!(
        "follow-up: {} ({})",
        crate::queue::title_from(&first.title, 80),
        location(first)
    );
    let mut s = format!(
        "This is a follow-up to a change that was already merged: run {run} merged \
         {pr_url} while the review finding(s) below were still open. Branch from the \
         current main - do not use the merged branch - and fix the defect described, \
         keeping the change small. If a finding no longer applies to current main, say so \
         and change nothing for it.\n\n\
         Merged run: {run}\nPull request: {pr_url}\n",
        run = state.id,
    );
    if let Some(t) = &origin_task {
        s.push_str(&format!("Original task: {t}\n"));
    }
    s.push_str("\n## Findings\n");
    for f in group {
        s.push_str(&format!(
            "\n### {} [{:?}] {}\n\nLocation: {}\n\n{}\n",
            f.id,
            f.severity,
            f.title,
            location(f),
            f.detail.trim()
        ));
    }
    let mut task = Task::new(
        title,
        s,
        state.repo.clone(),
        Source::Agent {
            run: state.id.clone(),
            node: NODE.to_owned(),
        },
    );
    task.id = task_id(&state.id, &ids);
    task.solo = true;
    task.followup = Some(FollowUp {
        run: state.id.clone(),
        origin_task,
        pr: pr_url.to_owned(),
        findings: ids,
        generation,
    });
    task
}

fn comment_body(state: &RunState, out: &Outcome) -> String {
    let mut s = format!(
        "<!-- magi-followup run={} -->\nThis pull request merged with review findings still \
         open. They were filed as follow-up tasks:\n\n",
        state.id
    );
    for r in &state.followups {
        s.push_str(&format!("- `{}`: {}\n", r.task, r.findings.join(", ")));
    }
    if !out.unfiled.is_empty() {
        s.push_str(&format!(
            "\nMinor/nit findings that were only listed, not filed: {}\n",
            out.unfiled.join(", ")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::queue::Queue;
    use crate::run::ReviewRecord;
    use crate::run::{Origin, ReviewRound, RunState};
    use crate::verdict::{Finding, ReviewVote, Severity};
    use std::path::PathBuf;

    fn finding(id: &str, sev: Severity, file: &str, line: u32) -> Finding {
        Finding {
            id: id.to_owned(),
            severity: sev,
            file: Some(file.to_owned()),
            line: Some(line),
            title: format!("title {id}"),
            detail: format!("detail of {id}"),
        }
    }

    fn record(seat: usize, findings: Vec<Finding>, vote: ReviewVote) -> ReviewRecord {
        ReviewRecord {
            attempts: 0,
            reviewer: seat,
            agent: "alpha".to_owned(),
            summary: String::new(),
            findings,
            vote: Some(vote),
            failed: None,
            duration_ms: 0,
        }
    }

    fn round(reviews: Vec<ReviewRecord>) -> ReviewRound {
        ReviewRound {
            round: 3,
            head: "deadbee".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews,
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 2,
            expected: 2,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }
    }

    fn merged(reviews: Vec<ReviewRecord>) -> RunState {
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc1234".to_owned(),
            "do it".to_owned(),
            Config::default(),
        );
        s.reviews.push(round(reviews));
        s
    }

    fn two_seats() -> Vec<ReviewRecord> {
        vec![
            record(
                1,
                vec![
                    finding("R3-1-1", Severity::Major, "src/waiter.rs", 211),
                    finding("R3-1-2", Severity::Minor, "src/a.rs", 1),
                ],
                ReviewVote::Reject,
            ),
            record(
                2,
                vec![
                    finding("R3-2-1", Severity::Major, "src/waiter.rs", 213),
                    finding("R3-2-2", Severity::Nit, "src/b.rs", 9),
                ],
                ReviewVote::Approve,
            ),
        ]
    }

    #[test]
    fn one_task_per_distinct_defect_with_the_findings_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        let out = file(&mut s, "https://example.invalid/o/r/pull/9", &q).unwrap();
        // waiter.rs pair is one defect; seat 1 rejected so its Minor is filed
        // too; seat 2's Nit is only listed.
        assert_eq!(out.filed.len(), 2, "{out:?}");
        assert_eq!(out.unfiled, vec!["R3-2-2".to_owned()]);
        let tasks = q.list();
        let t = tasks
            .iter()
            .find(|t| t.followup.as_ref().unwrap().findings.len() == 2)
            .unwrap();
        assert!(t.solo);
        assert_eq!(t.followup.as_ref().unwrap().generation, 1);
        assert_eq!(
            t.source,
            Source::Agent {
                run: s.id.clone(),
                node: NODE.to_owned()
            }
        );
        for needle in [
            "R3-1-1",
            "R3-2-1",
            "src/waiter.rs:211",
            "detail of R3-2-1",
            "pull/9",
            &s.id,
            "current main",
        ] {
            assert!(
                t.instruction.contains(needle),
                "{needle}: {}",
                t.instruction
            );
        }
        assert!(
            s.events
                .iter()
                .any(|e| e.message.contains("filed 2 follow-up"))
        );
        assert_eq!(s.followups.len(), 2);
    }

    #[test]
    fn filing_twice_does_not_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        file(&mut s, "u", &q).unwrap();
        let again = file(&mut s, "u", &q).unwrap();
        assert!(again.filed.is_empty());
        assert_eq!(q.list().len(), 2);
        // Even with the run's own record lost, the queue says what exists.
        s.followups.clear();
        let third = file(&mut s, "u", &q).unwrap();
        assert!(third.filed.is_empty());
        assert_eq!(q.list().len(), 2);
    }

    #[test]
    fn a_clean_merge_files_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(vec![record(
            1,
            vec![finding("R1-1-1", Severity::Minor, "a.rs", 1)],
            ReviewVote::Approve,
        )]);
        let out = file(&mut s, "u", &q).unwrap();
        assert!(out.filed.is_empty());
        assert!(q.list().is_empty());
        assert!(s.followups.is_empty());
    }

    #[test]
    fn a_failing_queue_is_an_error_event_not_a_status_change() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("queue");
        std::fs::write(&root, "not a directory").unwrap();
        let q = Queue::at(root);
        let mut s = merged(two_seats());
        s.status = crate::run::RunStatus::Merged;
        let _ = file(&mut s, "u", &q);
        assert_eq!(s.status, crate::run::RunStatus::Merged);
        assert!(
            s.events
                .iter()
                .any(|e| e.node == NODE && e.message.contains("stopped"))
        );
        assert!(s.followups.is_empty());
    }

    #[test]
    fn the_generation_cap_stops_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut parent = Task::new(
            "p".to_owned(),
            "i".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        parent.followup = Some(FollowUp {
            run: "r0".to_owned(),
            origin_task: None,
            pr: "u".to_owned(),
            findings: vec!["R1-1-1".to_owned()],
            generation: MAX_FOLLOWUP_GENERATION,
        });
        q.put(&mut parent).unwrap();
        let mut s = merged(two_seats());
        s.origin = Some(Origin {
            by: crate::run::StartedBy::Operator,
            task: Some(parent.id.clone()),
        });
        let out = file(&mut s, "u", &q).unwrap();
        assert!(out.filed.is_empty());
        assert!(!out.capped.is_empty());
        assert_eq!(q.list().len(), 1);
        assert!(
            s.events
                .iter()
                .any(|e| e.message.contains("generation cap"))
        );
    }

    #[test]
    fn a_deeper_task_files_one_generation_further() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut parent = Task::new(
            "p".to_owned(),
            "i".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        q.put(&mut parent).unwrap();
        let mut s = merged(two_seats());
        s.origin = Some(Origin {
            by: crate::run::StartedBy::Operator,
            task: Some(parent.id.clone()),
        });
        file(&mut s, "u", &q).unwrap();
        let t = q.list().into_iter().find(|t| t.followup.is_some()).unwrap();
        assert_eq!(t.followup.unwrap().origin_task, Some(parent.id));
    }
}
