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
    /// Findings not filed because another task already covers them, as
    /// `(finding id, covering task id)`. Recomputed from the queue each pass.
    pub covered: Vec<(String, String)>,
}

/// File follow-ups for a run that just merged, and comment on the pull
/// request. Never fails: problems become run events.
pub async fn after_merge(state: &mut RunState, pr_url: &str) {
    let queue = Queue::open();
    let release = state.config.graph.release_deputy_followups;
    // The deputy's tasks are settled first and again after filing, so the
    // outcome is the same whichever side got to the queue first.
    settle_deputy_tasks(state, pr_url, &queue, release);
    let outcome = if state.config.graph.file_followups {
        match file(state, pr_url, &queue) {
            Ok(o) => o,
            Err(e) => {
                state.event(NODE, format!("follow-up filing failed: {e:#}"));
                return;
            }
        }
    } else {
        Outcome::default()
    };
    if state.config.graph.file_followups && !release {
        settle_deputy_tasks(state, pr_url, &queue, false);
    }
    if outcome.filed.is_empty()
        && (state.followup_commented
            || (state.followups.is_empty()
                && outcome.covered.is_empty()
                && state.deputy_followups.is_empty()))
    {
        return;
    }
    let body = comment_body(state, &outcome);
    match crate::bump::gh_pr_comment(state, pr_url, &body).await {
        Ok(()) => state.followup_commented = true,
        Err(e) => state.event(NODE, format!("follow-up comment not posted: {e:#}")),
    }
}

/// Reason prefixes that mark a deputy task this module already judged and
/// left held. Both stop a second pass from rewriting the reason again.
const SUPERSEDED: &str = "[superseded] ";
const CAPPED: &str = "[generation cap] ";

/// A task the merge approval's deputy filed for run `run` that is still held
/// for pull request `pr_url`: the only kind [`settle_deputy_tasks`] touches.
/// Concrete identifiers only - the filer is the deputy seat of this run and
/// the hold reason names the pull request - never text similarity alone.
fn is_deputy_candidate(t: &Task, run: &str, pr_url: &str) -> bool {
    t.status == crate::queue::TaskStatus::Held
        && t.followup.is_none()
        && matches!(&t.source, Source::Agent { run: r, node }
            if r == run && node == crate::deputy::NODE)
        && t.hold_reason.as_deref().is_some_and(|r| {
            !r.starts_with(SUPERSEDED) && !r.starts_with(CAPPED) && names_pr(r, pr_url)
        })
}

/// Does deputy task `t` talk about finding `f`: its id as a word, a
/// `file:line` within [`LINE_WINDOW`] of the finding, or the finding's title
/// (normalized; contained only when long enough not to match by accident).
fn task_matches_finding(t: &Task, f: &Finding) -> bool {
    let text = format!("{}\n{}", t.title, t.instruction);
    if names_word(&text, &f.id) {
        return true;
    }
    if let (Some(file), Some(line)) = (&f.file, f.line)
        && text.match_indices(file.as_str()).any(|(i, _)| {
            text[i + file.len()..]
                .strip_prefix(':')
                .map(|r| {
                    r.chars()
                        .take_while(char::is_ascii_digit)
                        .collect::<String>()
                })
                .and_then(|d| d.parse::<u32>().ok())
                .is_some_and(|n| n.abs_diff(line) <= LINE_WINDOW)
        })
    {
        return true;
    }
    let (nt, nf) = (normalize(&t.title), normalize(&f.title));
    !nf.is_empty() && (nt == nf || (nf.split(' ').count() >= 3 && nt.contains(&nf)))
}

/// The follow-up depth of the task the run served; see [`file`].
fn parent_generation(state: &mut RunState, queue: &Queue, origin_task: Option<&str>) -> u32 {
    match state.followup_generation {
        Some(g) => g,
        None => match origin_task.map(|t| queue.get(t)) {
            Some(Ok(t)) => {
                let g = t.followup.map_or(0, |f| f.generation);
                state.followup_generation = Some(g);
                g
            }
            Some(Err(_)) => MAX_FOLLOWUP_GENERATION,
            None => 0,
        },
    }
}

/// Reconcile the follow-up tasks the merge approval's deputy filed (held,
/// reason naming this pull request) with the automatic ones, once the merge
/// is confirmed. Idempotent, and the same whichever side reached the queue
/// first:
///
/// - a deputy task that covers a finding an automatic follow-up already
///   covers (same finding, same file within [`LINE_WINDOW`] lines, or the same
///   normalized title) never runs twice: if the automatic one is queued the
///   owner's task wins and the automatic one is held with the reason, else
///   the deputy task stays held with a `[superseded]` reason. Nothing is
///   deleted;
/// - otherwise, with `release`, the task is released in the same queue write
///   that stamps it with a [`FollowUp`] (so it is released once, counts
///   toward the generation cap, and covers its findings for [`file`]);
/// - a task at the generation cap stays held with a `[generation cap]`
///   reason.
///
/// Only [`is_deputy_candidate`] tasks are touched, re-checked under the
/// queue's lock. Best-effort: failures are run events.
pub fn settle_deputy_tasks(state: &mut RunState, pr_url: &str, queue: &Queue, release: bool) {
    let tasks = queue.list();
    // The queue is the record: restore what a crash after the write lost.
    for t in &tasks {
        if t.followup.as_ref().is_some_and(|f| f.run == state.id)
            && matches!(&t.source, Source::Agent { node, .. } if node == crate::deputy::NODE)
            && !state.deputy_followups.contains(&t.id)
        {
            state.deputy_followups.push(t.id.clone());
        }
    }
    let cands: Vec<&Task> = tasks
        .iter()
        .filter(|t| is_deputy_candidate(t, &state.id, pr_url))
        .collect();
    if cands.is_empty() {
        return;
    }
    let chosen = state
        .reviews
        .last()
        .map(|r| select(r).0)
        .unwrap_or_default();
    let groups = group_findings(chosen);
    let autos: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            t.followup.as_ref().is_some_and(|f| f.run == state.id)
                && matches!(&t.source, Source::Agent { node, .. } if node == NODE)
        })
        .collect();
    let origin_task = state.origin.as_ref().and_then(|o| o.task.clone());
    let mut gen_of_parent = None;
    for cand in cands {
        // Findings the task covers, widened to whole defect groups.
        let matched: Vec<String> = groups
            .iter()
            .filter(|g| g.iter().any(|f| task_matches_finding(cand, f)))
            .flat_map(|g| g.iter().map(|f| f.id.clone()))
            .collect();
        let rivals: Vec<&&Task> = autos
            .iter()
            .filter(|a| {
                a.followup
                    .as_ref()
                    .is_some_and(|f| f.findings.iter().any(|x| matched.contains(x)))
            })
            .collect();
        if release {
            let parent_gen = *gen_of_parent
                .get_or_insert_with(|| parent_generation(state, queue, origin_task.as_deref()));
            if parent_gen >= MAX_FOLLOWUP_GENERATION {
                let why = format!(
                    "{CAPPED}generation {parent_gen} reached, not released after {pr_url} merged; was: {}",
                    cand.hold_reason.as_deref().unwrap_or_default()
                );
                mark_held(state, queue, cand, why, pr_url, "generation cap");
                continue;
            }
        }
        if !rivals.is_empty() {
            // Hold every idle rival under its claim, so the daemon cannot be
            // between reading it and starting it. Any rival that is not
            // idle, or cannot be claimed, means the automatic path owns the
            // finding and the deputy task is the one left held.
            let mut guards = Vec::new();
            let mut blocker: Option<&Task> = None;
            if release {
                for a in &rivals {
                    if a.status == crate::queue::TaskStatus::Held {
                        continue;
                    }
                    match queue.claim(&a.id) {
                        Ok(g) => guards.push((a.id.clone(), g)),
                        Err(_) => {
                            blocker = Some(a);
                            break;
                        }
                    }
                }
                if blocker.is_none() {
                    blocker = rivals.iter().map(|a| &***a).find(|a| {
                        !matches!(
                            queue.get(&a.id).map(|t| t.status),
                            Ok(crate::queue::TaskStatus::Queued | crate::queue::TaskStatus::Held)
                        )
                    });
                }
            } else {
                blocker = Some(&***rivals.first().expect("non-empty"));
            }
            if let Some(a) = blocker {
                drop(guards);
                let why = format!(
                    "{SUPERSEDED}follow-up task {} ({:?}) already covers {} of {pr_url}; was: {}",
                    a.id,
                    a.status,
                    matched.join(", "),
                    cand.hold_reason.as_deref().unwrap_or_default()
                );
                mark_held(state, queue, cand, why, pr_url, "superseded");
                continue;
            }
            // The owner's task wins; the idle automatic ones are held. Only
            // when every one is confirmed held is the deputy task released.
            let mut failed: Option<String> = None;
            for (id, _guard) in &guards {
                let why = format!(
                    "{SUPERSEDED}the owner's approval deputy filed task {} for the same finding(s) of {pr_url}",
                    cand.id
                );
                let held = queue.modify(id, |t| {
                    if t.status != crate::queue::TaskStatus::Queued {
                        return false;
                    }
                    t.hold_manual(Some(why));
                    true
                });
                match held {
                    Ok(true) => state.event(
                        NODE,
                        format!(
                            "follow-up task {id} held: superseded by deputy task {}",
                            cand.id
                        ),
                    ),
                    Ok(false) => {
                        failed = Some(id.clone());
                        break;
                    }
                    Err(e) => {
                        state.event(NODE, format!("could not hold follow-up {id}: {e:#}"));
                        failed = Some(id.clone());
                        break;
                    }
                }
            }
            if let Some(id) = failed {
                drop(guards);
                let why = format!(
                    "{SUPERSEDED}follow-up task {id} could not be held, so it may still run for {} of {pr_url}; was: {}",
                    matched.join(", "),
                    cand.hold_reason.as_deref().unwrap_or_default()
                );
                mark_held(state, queue, cand, why, pr_url, "superseded");
                continue;
            }
        } else if !release {
            continue;
        }
        let parent_gen = gen_of_parent.expect("computed when releasing");
        let stamp = FollowUp {
            run: state.id.clone(),
            origin_task: origin_task.clone(),
            pr: pr_url.to_owned(),
            findings: matched,
            generation: parent_gen + 1,
        };
        let id = cand.id.clone();
        let run = state.id.clone();
        let result = queue.modify(&id, |t| {
            if !is_deputy_candidate(t, &run, pr_url) {
                return false;
            }
            t.release();
            t.followup = Some(stamp);
            true
        });
        match result {
            Ok(true) => {
                if !state.deputy_followups.contains(&id) {
                    state.deputy_followups.push(id.clone());
                }
                state.event(
                    NODE,
                    format!("released deputy follow-up task {id} after {pr_url} merged"),
                );
            }
            Ok(false) => {}
            Err(e) => state.event(NODE, format!("could not release deputy task {id}: {e:#}")),
        }
    }
}

/// Rewrite a still-candidate deputy task's hold reason, leaving it held.
fn mark_held(
    state: &mut RunState,
    queue: &Queue,
    cand: &Task,
    why: String,
    pr_url: &str,
    kind: &str,
) {
    let run = state.id.clone();
    let reason = why.clone();
    match queue.modify(&cand.id, |t| {
        if !is_deputy_candidate(t, &run, pr_url) {
            return false;
        }
        t.hold_reason = Some(reason);
        true
    }) {
        Ok(true) => state.event(
            NODE,
            format!("deputy task {} left held ({kind}): {why}", cand.id),
        ),
        Ok(false) => {}
        Err(e) => state.event(
            NODE,
            format!("could not mark deputy task {}: {e:#}", cand.id),
        ),
    }
}

/// Findings the last review round leaves to be followed up, and the ids of
/// those only listed: every Major-or-above finding, plus all findings of a
/// seat whose final vote was reject.
fn select(round: &crate::run::ReviewRound) -> (Vec<Finding>, Vec<String>) {
    let rejecters: Vec<usize> = round
        .final_votes()
        .into_iter()
        .filter(|(_, _, v)| *v == ReviewVote::Reject)
        .map(|(seat, _, _)| seat)
        .collect();
    let (mut chosen, mut unfiled) = (Vec::new(), Vec::new());
    for rec in &round.reviews {
        let rejected = rejecters.contains(&rec.reviewer);
        for f in &rec.findings {
            if f.severity.blocks() || rejected {
                chosen.push(f.clone());
            } else {
                unfiled.push(f.id.clone());
            }
        }
    }
    (chosen, unfiled)
}

/// The deterministic half of [`after_merge`]: select, group, and file into
/// `queue`. Records the event and the run's own bookkeeping; the caller
/// saves the state.
pub fn file(state: &mut RunState, pr_url: &str, queue: &Queue) -> Result<Outcome> {
    let mut out = Outcome::default();
    let Some(round) = state.reviews.last().cloned() else {
        return Ok(out);
    };
    let (chosen, unfiled) = select(&round);
    out.unfiled = unfiled;
    if chosen.is_empty() {
        return Ok(out);
    }

    let origin_task = state.origin.as_ref().and_then(|o| o.task.clone());
    // The depth recorded when the run started wins; the queue is only a
    // fallback for runs that predate it, and an unknown depth is read as
    // unbounded-until-proven-otherwise only when there is no origin task.
    let parent_gen = parent_generation(state, queue, origin_task.as_deref());
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
            done.extend(f.findings.iter().cloned());
            let by_deputy =
                matches!(&t.source, Source::Agent { node, .. } if node == crate::deputy::NODE);
            if !by_deputy && !state.followups.iter().any(|r| r.task == t.id) {
                state.followups.push(FollowupRecord {
                    task: t.id,
                    findings: f.findings,
                });
            }
        }
    }

    let tasks = queue.list();
    let origin_chat = state.origin_chat.clone().or_else(|| {
        let tasks = queue.list();
        let parent = origin_task
            .as_deref()
            .and_then(|id| tasks.iter().find(|t| t.id == id))?;
        crate::consult::chat_talk_of(&tasks, parent)
    });
    let mut failure = None;
    for group in group_findings(chosen) {
        let ids: Vec<String> = group.iter().map(|f| f.id.clone()).collect();
        let own = task_id(&state.id, &ids);
        let covers: Vec<Option<String>> = ids
            .iter()
            .map(|id| covering_task(&tasks, &own, &state.id, pr_url, id))
            .collect();
        if covers.iter().all(Option::is_some) {
            for (id, by) in ids.iter().zip(covers.into_iter().flatten()) {
                state.event(
                    NODE,
                    format!("not filing follow-up for {id}: already covered by task {by}"),
                );
                out.covered.push((id.clone(), by));
            }
            continue;
        }
        if ids.iter().any(|id| done.contains(id)) {
            continue;
        }
        let mut task = build_task(state, pr_url, origin_task.clone(), &group, parent_gen + 1);
        task.origin_chat = origin_chat.clone();
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

/// The id of a task, other than `own`, that already covers finding `id` of
/// run `run` merged as `pr_url`, whatever its status. Concrete identifiers
/// only: a follow-up of the same run listing the finding, or an instruction
/// naming both the finding id and the pull request (URL or `#<number>`).
fn covering_task(tasks: &[Task], own: &str, run: &str, pr_url: &str, id: &str) -> Option<String> {
    tasks
        .iter()
        .find(|t| {
            t.id != own
                && (t
                    .followup
                    .as_ref()
                    .is_some_and(|f| f.run == run && f.findings.iter().any(|x| x == id))
                    || (names_word(&t.instruction, id) && names_pr(&t.instruction, pr_url)))
        })
        .map(|t| t.id.clone())
}

/// ASCII only: an id is often followed directly by prose in another script
/// (`R3-1-1を修正`), which must not read as part of the id.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// `needle` in `text` with no name character on either side.
fn names_word(text: &str, needle: &str) -> bool {
    !needle.is_empty()
        && text.match_indices(needle).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + needle.len()..].chars().next();
            before.is_none_or(|c| !is_name_char(c)) && after.is_none_or(|c| !is_name_char(c))
        })
}

/// Does `text` name the pull request by its URL or its `#<number>` form,
/// with no further digit (or name character before `#`) so `#47` is not `#473`?
fn names_pr(text: &str, pr_url: &str) -> bool {
    let url = pr_url.trim_end_matches('/');
    let digit_after = |rest: &str| rest.chars().next().is_some_and(|c| c.is_ascii_digit());
    if !url.is_empty()
        && text
            .match_indices(url)
            .any(|(i, _)| !digit_after(&text[i + url.len()..]))
    {
        return true;
    }
    let Some(n) = url
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    else {
        return false;
    };
    let tag = format!("#{n}");
    text.match_indices(&tag).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        before.is_none_or(|c| !is_name_char(c)) && !digit_after(&text[i + tag.len()..])
    })
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
        "<!-- magi-followup run={} -->\nThis pull request merged with review findings still open.",
        state.id
    );
    if state.followups.is_empty() {
        if !out.covered.is_empty() || state.deputy_followups.is_empty() {
            s.push_str(" Each is already covered by another task:\n");
        }
    } else {
        s.push_str(" They were filed as follow-up tasks:\n\n");
        for r in &state.followups {
            s.push_str(&format!("- `{}`: {}\n", r.task, r.findings.join(", ")));
        }
        if !out.covered.is_empty() {
            s.push_str("\nNot filed, already covered:\n");
        }
    }
    if !out.covered.is_empty() {
        s.push('\n');
    }
    for (id, by) in &out.covered {
        s.push_str(&format!("- {id}: already covered by task `{by}`\n"));
    }
    if !state.deputy_followups.is_empty() {
        s.push_str(
            "\nFollow-up tasks filed by the approval deputy and released after the merge:\n\n",
        );
        for id in &state.deputy_followups {
            s.push_str(&format!("- `{id}`\n"));
        }
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
        assert_eq!(s.followups.len(), 2, "records are restored from the queue");
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
        s.origin = Some(Origin {
            by: crate::run::StartedBy::Operator,
            task: Some("gone".to_owned()),
        });
        s.followup_generation = Some(MAX_FOLLOWUP_GENERATION);
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

    #[test]
    fn the_chat_is_inherited_from_the_run_even_when_the_parent_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.origin = Some(Origin {
            by: crate::run::StartedBy::Operator,
            task: Some("gone".to_owned()),
        });
        s.followup_generation = Some(1);
        s.origin_chat = Some("talk-1".to_owned());
        file(&mut s, "u", &q).unwrap();
        let t = q.list().into_iter().find(|t| t.followup.is_some()).unwrap();
        assert_eq!(t.origin_chat.as_deref(), Some("talk-1"));
    }

    #[test]
    fn a_parent_without_the_field_passes_on_what_its_ancestry_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut parent = Task::new(
            "p".to_owned(),
            "i".to_owned(),
            PathBuf::from("/repo"),
            Source::Agent {
                run: "talk-2".to_owned(),
                node: crate::queue::CHAT_NODE.to_owned(),
            },
        );
        parent.origin_chat = None;
        q.put(&mut parent).unwrap();
        let mut s = merged(two_seats());
        s.origin = Some(Origin {
            by: crate::run::StartedBy::Operator,
            task: Some(parent.id.clone()),
        });
        file(&mut s, "u", &q).unwrap();
        let t = q.list().into_iter().find(|t| t.followup.is_some()).unwrap();
        assert_eq!(t.origin_chat.as_deref(), Some("talk-2"));
    }

    const PR: &str = "https://github.com/o/r/pull/473";

    fn manual(text: &str) -> Task {
        Task::new(
            "manual".to_owned(),
            text.to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        )
    }

    /// File follow-ups with one manual task already in the queue.
    fn filed_with(text: &str) -> (Outcome, RunState, Queue, String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut t = manual(text);
        t.status = crate::queue::TaskStatus::Done;
        q.put(&mut t).unwrap();
        let mut s = merged(two_seats());
        let out = file(&mut s, PR, &q).unwrap();
        (out, s, q, t.id, dir)
    }

    #[test]
    fn a_task_naming_the_pr_url_and_ids_covers_the_group() {
        let (out, s, _q, id, _d) = filed_with(&format!("Fix R3-1-1 and R3-2-1 from {PR}"));
        // the waiter.rs group is covered; the other seat-1 Minor is still filed
        assert_eq!(out.filed.len(), 1, "{out:?}");
        assert_eq!(
            out.covered,
            vec![
                ("R3-1-1".to_owned(), id.clone()),
                ("R3-2-1".to_owned(), id.clone())
            ]
        );
        assert!(out.capped.is_empty());
        assert!(
            s.events
                .iter()
                .any(|e| e.message.contains("already covered"))
        );
        let body = comment_body(&s, &out);
        assert!(body.contains(&format!("R3-1-1: already covered by task `{id}`")));
    }

    #[test]
    fn the_hash_form_covers_but_a_prefix_number_does_not() {
        let (out, ..) = filed_with("R3-1-1 R3-2-1 fixed in PR #473");
        assert_eq!(out.covered.len(), 2);
        let (out, ..) = filed_with("R3-1-1 R3-2-1 fixed in #47");
        assert!(out.covered.is_empty());
        assert_eq!(out.filed.len(), 2);
        let (out, ..) = filed_with("R3-1-1 R3-2-1 fixed in #4731");
        assert!(out.covered.is_empty());
    }

    #[test]
    fn an_id_without_the_pr_does_not_cover() {
        let (out, ..) = filed_with("R3-1-1 and R3-2-1 are bad");
        assert!(out.covered.is_empty());
        assert_eq!(out.filed.len(), 2);
    }

    #[test]
    fn an_id_prefix_does_not_cover() {
        let (out, ..) = filed_with(&format!("R3-1-10 R3-2-10 in {PR}"));
        assert!(out.covered.is_empty());
    }

    #[test]
    fn partial_coverage_still_files() {
        let (out, ..) = filed_with(&format!("R3-1-1 in {PR}"));
        assert!(out.covered.is_empty());
        assert_eq!(out.filed.len(), 2);
    }

    #[test]
    fn another_followups_findings_cover() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        let mut other = manual("x");
        other.followup = Some(FollowUp {
            run: s.id.clone(),
            origin_task: None,
            pr: PR.to_owned(),
            findings: vec!["R3-1-1".to_owned(), "R3-2-1".to_owned()],
            generation: 1,
        });
        q.put(&mut other).unwrap();
        let out = file(&mut s, PR, &q).unwrap();
        assert_eq!(out.covered.len(), 2, "{out:?}");
        assert_eq!(out.filed.len(), 1);
    }

    #[test]
    fn github_text_fixed_followup_comment_passes() {
        let state = merged(two_seats());
        let body = comment_body(&state, &Outcome::default());
        assert!(crate::github_text::check("", &body).is_empty());
    }

    #[test]
    fn a_fully_covered_comment_lists_only_the_covered() {
        let (out, mut s, ..) = filed_with(&format!("R3-1-1 R3-2-1 {PR}"));
        s.followups.clear();
        let body = comment_body(&s, &out);
        assert!(body.contains("already covered by task"));
        assert!(!body.contains("filed as follow-up"));
    }

    #[test]
    fn an_id_next_to_japanese_prose_still_covers() {
        let (out, ..) = filed_with("PR #473 の R3-1-1を修正、R3-2-1も対応");
        assert_eq!(out.covered.len(), 2, "{out:?}");
    }

    // ---- deputy tasks released after the merge ----

    fn deputy_task(run: &str, title: &str, text: &str, reason: &str) -> Task {
        let mut t = Task::new(
            title.to_owned(),
            text.to_owned(),
            PathBuf::from("/repo"),
            Source::Agent {
                run: run.to_owned(),
                node: crate::deputy::NODE.to_owned(),
            },
        );
        t.hold_manual(Some(reason.to_owned()));
        t
    }

    fn status_of(q: &Queue, id: &str) -> crate::queue::TaskStatus {
        q.get(id).unwrap().status
    }

    #[test]
    fn a_deputy_task_is_released_once_and_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        let mut t = deputy_task(
            &s.id,
            "Unrelated work",
            "do the thing",
            &format!("after {PR}"),
        );
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, true);
        let got = q.get(&t.id).unwrap();
        assert_eq!(got.status, crate::queue::TaskStatus::Queued);
        assert!(got.hold_reason.is_none());
        let f = got.followup.unwrap();
        assert_eq!((f.run.as_str(), f.generation), (s.id.as_str(), 1));
        assert_eq!(s.deputy_followups, vec![t.id.clone()]);
        let events = s.events.len();
        // A second pass, or a restart that lost the run's record, changes nothing.
        settle_deputy_tasks(&mut s, PR, &q, true);
        assert_eq!(s.events.len(), events);
        s.deputy_followups.clear();
        settle_deputy_tasks(&mut s, PR, &q, true);
        assert_eq!(
            s.deputy_followups,
            vec![t.id.clone()],
            "rebuilt from the queue"
        );
        assert_eq!(status_of(&q, &t.id), crate::queue::TaskStatus::Queued);
    }

    #[test]
    fn unrelated_held_tasks_are_never_released() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        let mut other_run = deputy_task("someone-else", "a", "b", &format!("after {PR}"));
        let mut other_pr = deputy_task(&s.id, "c", "d", "after https://github.com/o/r/pull/4731");
        let mut no_reason = deputy_task(&s.id, "e", "f", "x");
        no_reason.hold_reason = None;
        let mut human = manual("g");
        human.hold_manual(Some(format!("waiting for {PR}")));
        let mut queued = deputy_task(&s.id, "h", "i", &format!("after {PR}"));
        queued.release();
        let ids: Vec<String> = [
            &mut other_run,
            &mut other_pr,
            &mut no_reason,
            &mut human,
            &mut queued,
        ]
        .into_iter()
        .map(|t| {
            q.put(t).unwrap();
            t.id.clone()
        })
        .collect();
        settle_deputy_tasks(&mut s, PR, &q, true);
        for id in &ids[..4] {
            assert_eq!(status_of(&q, id), crate::queue::TaskStatus::Held, "{id}");
            assert!(q.get(id).unwrap().followup.is_none());
        }
        assert!(s.deputy_followups.is_empty());
    }

    #[test]
    fn the_switch_off_leaves_the_task_held() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        let mut t = deputy_task(&s.id, "x", "y", &format!("after {PR}"));
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, false);
        assert_eq!(status_of(&q, &t.id), crate::queue::TaskStatus::Held);
    }

    #[test]
    fn the_generation_cap_applies_to_released_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(MAX_FOLLOWUP_GENERATION);
        let mut t = deputy_task(&s.id, "x", "y", &format!("after {PR}"));
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, true);
        let got = q.get(&t.id).unwrap();
        assert_eq!(got.status, crate::queue::TaskStatus::Held);
        assert!(got.hold_reason.unwrap().starts_with(CAPPED));
        let events = s.events.len();
        settle_deputy_tasks(&mut s, PR, &q, true);
        assert_eq!(s.events.len(), events, "marked once");
        let mut s2 = merged(two_seats());
        s2.followup_generation = Some(MAX_FOLLOWUP_GENERATION - 1);
        let mut t2 = deputy_task(&s2.id, "x", "y", &format!("after {PR}"));
        q.put(&mut t2).unwrap();
        settle_deputy_tasks(&mut s2, PR, &q, true);
        assert_eq!(
            q.get(&t2.id).unwrap().followup.unwrap().generation,
            MAX_FOLLOWUP_GENERATION
        );
    }

    #[test]
    fn deputy_first_the_owners_task_runs_and_the_automatic_one_is_not_filed() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        let mut t = deputy_task(
            &s.id,
            "Fix the waiter",
            "R3-1-1 at src/waiter.rs:212 loses replies",
            &format!("after {PR}"),
        );
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, true);
        let out = file(&mut s, PR, &q).unwrap();
        assert_eq!(out.covered.len(), 2, "{out:?}");
        // Only the seat-1 Minor group is filed; the waiter.rs defect has one task.
        assert_eq!(out.filed.len(), 1, "{out:?}");
        let runnable: Vec<_> = q
            .list()
            .into_iter()
            .filter(|t| {
                t.status == crate::queue::TaskStatus::Queued
                    && t.followup
                        .as_ref()
                        .is_some_and(|f| f.findings.iter().any(|x| x == "R3-1-1"))
            })
            .collect();
        assert_eq!(runnable.len(), 1);
        assert_eq!(runnable[0].id, t.id);
    }

    #[test]
    fn followup_first_a_queued_automatic_task_gives_way_to_the_owners() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        file(&mut s, PR, &q).unwrap();
        let mut t = deputy_task(
            &s.id,
            "Waiter replies",
            "see R3-2-1",
            &format!("after {PR}"),
        );
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, true);
        let all = q.list();
        let live: Vec<_> = all
            .iter()
            .filter(|t| {
                t.status == crate::queue::TaskStatus::Queued
                    && t.followup
                        .as_ref()
                        .is_some_and(|f| f.findings.iter().any(|x| x == "R3-2-1"))
            })
            .collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, t.id);
        let dropped = all
            .iter()
            .find(|a| {
                a.id != t.id
                    && a.followup
                        .as_ref()
                        .is_some_and(|f| f.findings.iter().any(|x| x == "R3-2-1"))
            })
            .unwrap();
        assert_eq!(dropped.status, crate::queue::TaskStatus::Held);
        assert!(
            dropped
                .hold_reason
                .as_ref()
                .unwrap()
                .starts_with(SUPERSEDED)
        );
    }

    #[test]
    fn followup_first_a_finished_automatic_task_supersedes_the_deputys() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        file(&mut s, PR, &q).unwrap();
        for mut a in q.list() {
            a.status = crate::queue::TaskStatus::Done;
            q.put(&mut a).unwrap();
        }
        // Matched by the finding's title, not its id.
        let mut t = deputy_task(&s.id, "title R3-1-1", "no ids here", &format!("after {PR}"));
        t.instruction = "nothing".to_owned();
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, true);
        let got = q.get(&t.id).unwrap();
        assert_eq!(got.status, crate::queue::TaskStatus::Held);
        assert!(got.followup.is_none());
        let reason = got.hold_reason.unwrap();
        assert!(reason.starts_with(SUPERSEDED), "{reason}");
        assert!(reason.contains(PR));
        let events = s.events.len();
        settle_deputy_tasks(&mut s, PR, &q, true);
        assert_eq!(s.events.len(), events, "judged once");
    }

    #[test]
    fn a_held_deputy_task_is_superseded_by_a_later_filing_when_the_switch_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        let mut t = deputy_task(&s.id, "x", "R3-1-1 here", &format!("after {PR}"));
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, false);
        assert!(
            !q.get(&t.id)
                .unwrap()
                .hold_reason
                .unwrap()
                .starts_with(SUPERSEDED)
        );
        file(&mut s, PR, &q).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, false);
        let got = q.get(&t.id).unwrap();
        assert_eq!(got.status, crate::queue::TaskStatus::Held);
        assert!(got.hold_reason.unwrap().starts_with(SUPERSEDED));
    }

    #[test]
    fn the_comment_lists_released_deputy_tasks() {
        let mut s = merged(two_seats());
        s.deputy_followups.push("abcd-1".to_owned());
        let body = comment_body(&s, &Outcome::default());
        assert!(body.contains("`abcd-1`"));
        assert!(!body.contains("already covered"));
        assert!(crate::github_text::check("", &body).is_empty());
    }

    #[test]
    fn a_deputy_task_over_two_automatic_ones_holds_both() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        file(&mut s, PR, &q).unwrap();
        let mut t = deputy_task(&s.id, "both", "R3-1-1 and R3-1-2", &format!("after {PR}"));
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, true);
        assert_eq!(status_of(&q, &t.id), crate::queue::TaskStatus::Queued);
        let live = q
            .list()
            .into_iter()
            .filter(|x| x.status == crate::queue::TaskStatus::Queued)
            .count();
        assert_eq!(live, 1, "only the owner's task still runs");
    }

    #[test]
    fn a_claimed_automatic_task_is_not_touched_and_the_deputys_stays_held() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut s = merged(two_seats());
        s.followup_generation = Some(0);
        file(&mut s, PR, &q).unwrap();
        let auto = q
            .list()
            .into_iter()
            .find(|x| {
                x.followup
                    .as_ref()
                    .is_some_and(|f| f.findings.iter().any(|i| i == "R3-1-1"))
            })
            .unwrap();
        let _claim = q.claim(&auto.id).unwrap();
        let mut t = deputy_task(&s.id, "x", "R3-1-1", &format!("after {PR}"));
        q.put(&mut t).unwrap();
        settle_deputy_tasks(&mut s, PR, &q, true);
        assert_eq!(status_of(&q, &auto.id), crate::queue::TaskStatus::Queued);
        let got = q.get(&t.id).unwrap();
        assert_eq!(got.status, crate::queue::TaskStatus::Held);
        assert!(got.hold_reason.unwrap().starts_with(SUPERSEDED));
    }
}
