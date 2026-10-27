//! Rebasing the winning branch onto a moved base, with a fixer for conflicts.
//!
//! Both `graph::Runner::sync_to_base` (before review) and `land::Step::Rebase`
//! (after the pull request exists) used to stop on a conflict and leave it to
//! a person. A conflict is often a chore - two `pub mod` lines on the same
//! spot - and what follows it (a rule the base gained meanwhile) is exactly
//! what the gate-fix round handles, so both callers now come through
//! [`rebase_with_fixer`], which hands the standing conflict to the fixer seat.
//!
//! The rules, none optional:
//!
//! - **magi resolves nothing itself.** No taking one side, no line merging:
//!   the agent decides. Everything here is scaffolding around that call.
//! - **The round is judged by what git says**, never by the fixer's report:
//!   the rebase is no longer in progress, nothing is unmerged, no conflict
//!   marker is left in a path that conflicted, and the base is an ancestor of
//!   the result (an agent that ran `git rebase --abort` leaves a tidy tree
//!   that contains no base at all). Whether the tree *builds* is left to the
//!   review and gate that follow, which already know how to fix a breakage.
//! - **The budget is `graph.review_rounds`, counted in
//!   `RunState::rebase_fixes` per sequencer stop** and saved before the fixer
//!   is called, shared by both callers, so a park or crash cannot hand a round back. It
//!   touches neither the task's attempts nor land's own rebase budget.
//! - **A failure restores the branch.** Whatever the fixer left, the branch
//!   ref goes back to where it was and the throwaway worktree is removed, so
//!   the fallback is exactly the old "conflict, a person decides".

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use jiff::Timestamp;

use crate::agent::{self, Invocation};
use crate::git;
use crate::land::seat_of;
use crate::prompt;
use crate::run::{QuotaLoss, RebaseFixRecord, RunState};

/// How [`rebase_with_fixer`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rebased {
    /// The branch now sits on top of the base (rebased cleanly or resolved by
    /// the fixer); the throwaway worktree is gone.
    Applied,
    /// It did not apply. The branch is untouched, the worktree is gone, and
    /// the string says what was tried: rounds spent, paths still conflicted,
    /// and what git said.
    Stopped(String),
}

/// Longest conflict excerpt shown to the fixer, per file and in total.
const HUNK_PER_FILE: usize = 4_000;
const HUNK_TOTAL: usize = 16_000;
/// Commit subjects listed per side.
const SUBJECTS: usize = 20;
/// Paths named in a failure reason before "and N more".
const PATHS_IN_REASON: usize = 8;

/// Rebase `branch` onto `onto` in `scratch`, giving a conflict to the fixer.
///
/// Re-entrant: when `scratch` already holds a rebase in progress (a run that
/// died in the middle of a round) it carries on from there instead of
/// starting over, and the rounds already recorded still count.
pub async fn rebase_with_fixer(
    state: &mut RunState,
    scratch: &Path,
    branch: &str,
    onto: &str,
) -> Result<Rebased> {
    let repo = state.repo.clone();
    let cap = state.config.graph.review_rounds;
    let orig = git::rev_parse(&repo, &format!("refs/heads/{branch}")).await?;

    let mut said = String::new();
    if !git::rebase_in_progress(scratch).await {
        match git::rebase_start(&repo, scratch, branch, onto).await? {
            git::RebaseStart::Applied => return Ok(Rebased::Applied),
            git::RebaseStart::Failed(why) => return Ok(Rebased::Stopped(why)),
            git::RebaseStart::Conflicted(why) => said = why,
        }
    }
    // A tracking ref may have advanced while this rebase was parked. Finish
    // the saved operation against its own target, then start an ordinary
    // base sync if the requested target has moved.
    let onto_sha = rebase_target(scratch).await?;
    // Every path that was ever unmerged: the marker check on the finished
    // tree looks at exactly these.
    let mut touched: Vec<String> = Vec::new();

    loop {
        if !git::rebase_in_progress(scratch).await {
            let result = finish(state, scratch, branch, &orig, &onto_sha, &touched, &said).await?;
            if result == Rebased::Applied && git::rev_parse(&repo, onto).await? != onto_sha {
                state.event("rebase", "saved rebase completed; syncing the newer base");
                state.save()?;
                return Box::pin(rebase_with_fixer(state, scratch, branch, onto)).await;
            }
            return Ok(result);
        }
        let paths = git::unmerged_paths(scratch).await.unwrap_or_default();
        for p in &paths {
            if !touched.contains(p) {
                touched.push(p.clone());
            }
        }
        // A fixer that already worked on *this* stop point may have left the
        // contents resolved with only the staging undone (a run killed between
        // the steps). Finish that before the budget is looked at:
        // re-confirming resolved files is not a conflict round.
        if let Launch::Advanced = launcher_stage(state, scratch, &orig, &onto_sha).await? {
            continue;
        }
        let spent = stop_rounds(state, scratch).await;
        if spent >= cap {
            let why = reason(spent, cap, &paths, &said, "the rounds are spent");
            abandon(&repo, scratch, branch, &orig).await;
            return Ok(Rebased::Stopped(why));
        }

        let winner = state
            .winner()
            .cloned()
            .context("resolving a rebase conflict needs a winning candidate")?;
        let roles = state
            .config
            .resolve_roles()
            .context("resolve the roster for the rebase fix")?;
        // The chain (`crate::fixer`): one round however many agents it takes.
        let attempts = crate::fixer::attempts(state, &roles, &winner);
        let ids: Vec<String> = attempts.iter().map(|(s, _)| s.id.clone()).collect();

        let round = spent + 1;
        let artifact_round = state.rebase_fixes.len() + 1;
        let branch_subjects = subjects(scratch, &format!("{onto}..{branch}")).await;
        let onto_subjects = subjects(scratch, &format!("{branch}..{onto}")).await;
        let hunks = hunks(scratch, &paths);
        let prompt_text = prompt::rebase_conflict(&prompt::RebaseConflict {
            instruction: &state.instruction,
            worktree: scratch,
            branch,
            onto,
            paths: &paths,
            branch_subjects: &branch_subjects,
            onto_subjects: &onto_subjects,
            hunks: &hunks,
            round,
            cap,
            language: &state.config.graph.language,
            github_language: &state.config.graph.github_language,
        });
        let prompt_text = if state.config.cache_dir().is_some() {
            format!("{prompt_text}\n\n{}", prompt::build_cache_note("fix", true))
        } else {
            prompt_text
        };

        // Spent before the call, on disk: a run killed mid-round must not
        // get the round back.
        let stop = rebase_head(scratch).await;
        let index_outside = Some(round_baseline(state, scratch, &paths).await?);
        state.rebase_fixes.push(RebaseFixRecord {
            agent: ids[0].clone(),
            paths: paths.clone(),
            from: Some(orig.clone()),
            finished: false,
            error: None,
            stop,
            index_outside,
            launcher: None,
            infra: None,
        });
        state.event(
            "rebase",
            format!(
                "{branch} conflicts with {onto} ({} path(s)); fixer round {round} of {cap}",
                paths.len()
            ),
        );
        state.save()?;

        // Each id at most once, forward only. The round was spent and saved
        // above, so a chain advance inside it spends nothing further and a
        // crash cannot hand it back; only the record's `agent` follows the
        // asked agent (saved before its call, like the round itself).
        let artifacts = agent::artifacts_dir(&state.dir());
        let mut last = None;
        for (i, (spec, seat_key)) in attempts.into_iter().enumerate() {
            if i > 0 {
                if let Some(r) = state.rebase_fixes.last_mut() {
                    r.agent = spec.id.clone();
                }
                state.save()?;
            }
            let mut seat = seat_of(state, &seat_key, &spec.id);
            let stem = if i == 0 {
                format!("rebase-fix-{artifact_round}")
            } else {
                format!("rebase-fix-{artifact_round}-{}", spec.id)
            };
            let out = agent::invoke(
                &spec,
                &mut seat,
                &Invocation {
                    coordination: state.config.coordination.as_ref(),
                    fetched_base: state.fetched_base.as_ref(),
                    cwd: scratch,
                    prompt: &prompt_text,
                    timeout: Duration::from_secs(state.config.graph.timeout_fix),
                    allow_write: true,
                    sessions: state.config.graph.sessions,
                    artifacts: &artifacts,
                    stem: &stem,
                    run: &state.id,
                    node: "rebase",
                    cache_dir: state.config.cache_dir().as_deref(),
                    attachments: &[],
                    writable: &[],
                },
            )
            .await;
            let seat_name = seat.key.clone();
            state.seats.insert(seat.key.clone(), seat);
            if let Some(next) = ids.get(i + 1)
                && agent::chain_advances(&out)
            {
                let (class, why) = crate::fixer::failure_of(&out);
                crate::graph::record_handover(
                    state, "rebase", &seat_name, &spec.id, next, &class, &why,
                );
                continue;
            }
            last = Some((out, seat_name));
            break;
        }
        let (out, seat_name) = last.expect("the fixer chain always has an entry");

        let mut error = None;
        let mut quota = false;
        match out {
            Ok(o) if o.quota_exhausted() => {
                state.quota.push(QuotaLoss {
                    seat: seat_name,
                    node: "rebase".to_owned(),
                    at: Timestamp::now(),
                    reset: o.quota.as_ref().and_then(|q| q.reset.clone()),
                });
                error = Some("rate limited (quota); the fixer could not run".to_owned());
                quota = true;
            }
            Ok(o) if !o.usable() => {
                error = Some(format!(
                    "the fixer produced nothing usable ({})",
                    crate::notices::turn_status(o.exit_code, o.timed_out, o.quota_exhausted())
                ));
            }
            Ok(_) => {}
            Err(e) => error = Some(format!("{e:#}")),
        }

        if let Some(r) = state.rebase_fixes.last_mut() {
            r.error = error.clone();
        }
        if !quota {
            // The seat resolved contents; the launcher owns Git's metadata.
            // A denied write is `Err` (a resumable stop, no further round).
            if let Launch::Nothing(why) = launcher_stage(state, scratch, &orig, &onto_sha).await? {
                state.event("rebase", format!("launcher staging skipped: {why}"));
            }
        }
        let finished = !git::rebase_in_progress(scratch).await;
        if let Some(r) = state.rebase_fixes.last_mut() {
            r.finished = finished;
        }
        state.save()?;

        if quota {
            // A retry now fails the same way; do not burn the rest.
            let paths = git::unmerged_paths(scratch).await.unwrap_or_default();
            let why = reason(
                state.rebase_fixes.len(),
                cap,
                &paths,
                &said,
                "the fixer hit its rate limit",
            );
            abandon(&repo, scratch, branch, &orig).await;
            return Ok(Rebased::Stopped(why));
        }
    }
}

/// What [`launcher_stage`] did.
#[derive(Debug)]
enum Launch {
    /// Nothing was staged or continued; why, for the event log.
    Nothing(String),
    /// The sequencer moved (continued to the end or to the next stop).
    Advanced,
}

/// The commit the sequencer is replaying right now.
async fn rebase_head(scratch: &Path) -> Option<String> {
    git::git(scratch, &["rev-parse", "-q", "--verify", "REBASE_HEAD"])
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

/// A denied or failed Git write is infrastructure: record it on the round,
/// keep the worktree, branch and resolved contents, and stop resumably.
fn infra_stop(state: &mut RunState, detail: String) -> anyhow::Error {
    if let Some(r) = state.rebase_fixes.last_mut() {
        r.infra = Some(detail.clone());
    }
    block(state, "git metadata write", detail)
}

fn block(state: &mut RunState, kind: &str, detail: String) -> anyhow::Error {
    let detail = format!("rebase recovery: {detail}");
    state.resource_block = Some(crate::run::ResourceBlock {
        cause: crate::run::ResourceBlockCause::Other,
        node: "rebase".into(),
        kind: kind.into(),
        detail: detail.clone(),
        at: Timestamp::now(),
    });
    state.event("resource", detail.clone());
    state.save().ok();
    crate::repo_lock::ResourceBlocked(detail).into()
}

/// Status entries `(XY, path)` of the worktree, renames split.
async fn status_entries(scratch: &Path) -> Result<Vec<(String, String)>> {
    let out = git::git_raw(
        scratch,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )
    .await?;
    anyhow::ensure!(out.ok(), "git status failed: {}", out.stderr);
    Ok(out
        .stdout
        .split('\0')
        .filter(|e| e.len() > 3)
        .map(|e| (e[..2].to_owned(), e[3..].to_owned()))
        .collect())
}

/// Index-side changes against HEAD outside `expected` (what Git applied
/// cleanly), as a comparable string.
async fn index_outside(scratch: &Path, expected: &[String]) -> Result<String> {
    let out = git::git_raw(
        scratch,
        &[
            "diff",
            "--cached",
            "--raw",
            "-z",
            "--no-renames",
            "--no-abbrev",
        ],
    )
    .await?;
    anyhow::ensure!(out.ok(), "git diff --cached failed: {}", out.stderr);
    Ok(outside_diff(&out.stdout, expected))
}

fn outside_diff(raw: &str, expected: &[String]) -> String {
    let mut parts = raw.split('\0').filter(|p| !p.is_empty());
    let mut lines = Vec::new();
    while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
        if !expected.iter().any(|e| e == path) {
            lines.push(format!("{meta}\t{path}"));
        }
    }
    lines.join("\n")
}

/// Does `last` belong to the stop point the sequencer is at now?
///
/// A record carries the replayed commit. A record from before that was kept
/// can only be matched by what it says about itself, so it counts only when
/// every recorded round saw the same non-empty conflict set and none knows
/// its stop: a different pick would not have repeated the same paths.
async fn same_stop(state: &RunState, scratch: &Path) -> bool {
    let Some(last) = state.rebase_fixes.last() else {
        return false;
    };
    match &last.stop {
        Some(stop) => rebase_head(scratch).await.as_deref() == Some(stop.as_str()),
        None => {
            !last.paths.is_empty()
                && state
                    .rebase_fixes
                    .iter()
                    .all(|r| r.stop.is_none() && r.paths == last.paths)
        }
    }
}

/// Records for the current stop form a contiguous suffix. Always retain the
/// first record's paths and index baseline, even after a rejected seat edit.
fn stop_records(state: &RunState) -> &[RebaseFixRecord] {
    let Some(last) = state.rebase_fixes.last() else {
        return &[];
    };
    let start = state
        .rebase_fixes
        .iter()
        .rposition(|r| r.stop != last.stop || (r.stop.is_none() && r.paths != last.paths))
        .map_or(0, |i| i + 1);
    &state.rebase_fixes[start..]
}

async fn stop_rounds(state: &RunState, scratch: &Path) -> usize {
    if same_stop(state, scratch).await {
        stop_records(state).len()
    } else {
        0
    }
}

async fn round_baseline(state: &mut RunState, scratch: &Path, paths: &[String]) -> Result<String> {
    if same_stop(state, scratch).await {
        if let Some(before) = &stop_records(state)[0].index_outside {
            return Ok(before.clone());
        }
        return Err(infra_stop(state,
            "the saved stop has no original outside-index baseline; keep the resolution and have the original owner establish a verified baseline before continuation".into()));
    }
    index_outside(scratch, paths)
        .await
        .map_err(|e| infra_stop(state, format!("cannot capture index baseline: {e:#}")))
}

/// Recompute the clean part of this pick without reading the fixer's index
/// as a baseline. Only object data is written; the retained index, contents,
/// branch and sequencer are untouched. Unsupported merge shapes fail closed.
async fn legacy_baseline(
    scratch: &Path,
    paths: &[String],
    lease: Option<crate::repo_lock::SeatLease>,
) -> Result<(String, String, String)> {
    let head = git::rev_parse(scratch, "HEAD").await?;
    let pick = git::rev_parse(scratch, "REBASE_HEAD").await?;
    let parent = git::rev_parse(scratch, "REBASE_HEAD^").await?;
    let out = mutate(
        scratch,
        &[
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            &format!("--merge-base={parent}"),
            &head,
            &pick,
        ],
        lease,
    )
    .await?;
    anyhow::ensure!(
        out.code == Some(1),
        "cannot reconstruct legacy conflict baseline: {}",
        out.stderr
    );
    let mut fields = out.stdout.split('\0').filter(|p| !p.is_empty());
    let tree = fields.next().context("merge-tree returned no tree")?;
    let mut conflicts: Vec<&str> = fields.collect();
    conflicts.sort_unstable();
    let mut expected: Vec<&str> = paths.iter().map(String::as_str).collect();
    expected.sort_unstable();
    anyhow::ensure!(
        conflicts == expected,
        "reconstructed conflict paths differ from the saved stop"
    );
    let out = git::git_raw(
        scratch,
        &[
            "diff",
            "--raw",
            "-z",
            "--no-renames",
            "--no-abbrev",
            &head,
            tree,
        ],
    )
    .await?;
    anyhow::ensure!(
        out.ok(),
        "cannot read reconstructed baseline: {}",
        out.stderr
    );
    Ok((outside_diff(&out.stdout, paths), head, pick))
}

async fn rebase_target(scratch: &Path) -> Result<String> {
    let path = git::git(scratch, &["rev-parse", "--git-path", "rebase-merge/onto"]).await?;
    let target = std::fs::read_to_string(scratch.join(path))?;
    git::rev_parse(scratch, &format!("{}^{{commit}}", target.trim())).await
}

async fn stop_identity(scratch: &Path, original: &str, onto: &str) -> Result<()> {
    for (name, expected) in [("orig-head", original), ("onto", onto)] {
        let path = git::git(
            scratch,
            &["rev-parse", "--git-path", &format!("rebase-merge/{name}")],
        )
        .await?;
        let path = Path::new(&path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            scratch.join(path)
        };
        anyhow::ensure!(
            std::fs::read_to_string(path)?.trim() == expected,
            "rebase {name} changed"
        );
    }
    Ok(())
}

// The command owns coordination until all hooks and descendants have stopped,
// even if its caller is cancelled while waiting for Git.
async fn mutate(
    scratch: &Path,
    args: &[&str],
    lease: Option<crate::repo_lock::SeatLease>,
) -> Result<git::GitOut> {
    use crate::proc::Quiet as _;
    use std::process::Stdio;
    use tokio::io::AsyncReadExt as _;
    let wait = crate::repo_lock::WaitPolicy::current();
    anyhow::ensure!(
        !wait.cancelled(),
        crate::repo_lock::ResourceBlocked("launcher Git operation cancelled".into())
    );
    let mut command = tokio::process::Command::new("git");
    command
        .quiet()
        .args(args)
        .current_dir(scratch)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let outputs = crate::retention::OutputLease::borrow(scratch)?
        .into_iter()
        .collect();
    let mut running = crate::seat_process::Running::spawn_operation(command, lease, outputs)?;
    let mut stdout = running.child().stdout.take().context("Git stdout")?;
    let mut stderr = running.child().stderr.take().context("Git stderr")?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let cancelled = async {
        while !wait.cancelled() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let (status, _, _) = tokio::select! {
        result = async { tokio::try_join!(
            running.child().wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        ) } => result?,
        () = cancelled => return Err(crate::repo_lock::ResourceBlocked("launcher Git operation cancelled".into()).into()),
    };
    running.finish().await;
    Ok(git::GitOut {
        code: status.code(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

/// Stage what the fixer resolved and continue the sequencer, as the launcher.
///
/// The seat only edits contents; staging and continuing stay with the
/// launcher. Everything here is re-derived from Git inside the
/// repository's coordination lease, so an interrupted pass is simply run
/// again: the current unmerged set must lie inside the round's recorded
/// paths, none may still hold markers, nothing outside them may have been
/// edited, and only then are exactly those paths staged (literal pathspecs)
/// and `rebase --continue` run with the recording exec untouched. Content
/// problems are `Nothing` (the ordinary round machinery goes on); a refused
/// Git write is an `Err` resource block that spends nothing and keeps the
/// worktree, the branch and the resolved files.
async fn launcher_stage(
    state: &mut RunState,
    scratch: &Path,
    original: &str,
    onto: &str,
) -> Result<Launch> {
    if !git::rebase_in_progress(scratch).await {
        return Ok(Launch::Nothing("no rebase in progress".into()));
    }
    if !same_stop(state, scratch).await {
        return Ok(Launch::Nothing(
            "no fixer round is recorded for this stop point".into(),
        ));
    }
    let wait = {
        let inherited = crate::repo_lock::WaitPolicy::current();
        crate::repo_lock::WaitPolicy::new(
            Duration::from_secs(state.config.graph.timeout_resource_wait),
            move || inherited.cancelled(),
        )
    };
    if wait.cancelled() {
        return Err(block(
            state,
            "repository coordination",
            "cancelled before staging; the resolved files are kept".into(),
        ));
    }
    let lease = crate::repo_lock::SeatLease::operation_with(
        scratch,
        state.config.coordination.as_ref(),
        &wait,
    )
    .await;
    let lease = match lease {
        Ok(l) => l,
        Err(e) if e.is::<crate::repo_lock::ResourceBlocked>() => {
            return Err(block(state, "repository coordination", e.to_string()));
        }
        Err(e) => return Err(e),
    };

    // Re-validated under the lease: the seat is gone, but an external
    // updater is not.
    if !git::rebase_in_progress(scratch).await || !same_stop(state, scratch).await {
        return Ok(Launch::Nothing("the stop point changed".into()));
    }
    if let Err(e) = stop_identity(scratch, original, onto).await {
        return Err(infra_stop(state, format!("{e:#}")));
    }
    let last = stop_records(state)[0].clone();
    let lease = if last.index_outside.is_none() {
        let (baseline, head, pick) = legacy_baseline(scratch, &last.paths, lease)
            .await
            .map_err(|e| infra_stop(state, format!("legacy baseline recovery: {e:#}")))?;
        // The mutation's cleanup owner held the previous lease. Reacquire
        // before accepting the result, then revalidate the exact stop/index.
        let lease = crate::repo_lock::SeatLease::operation_with(
            scratch,
            state.config.coordination.as_ref(),
            &wait,
        )
        .await
        .map_err(|e| block(state, "repository coordination", e.to_string()))?;
        let validation = async {
            stop_identity(scratch, original, onto).await?;
            anyhow::ensure!(
                same_stop(state, scratch).await
                    && git::rev_parse(scratch, "HEAD").await? == head
                    && rebase_head(scratch).await.as_deref() == Some(&pick),
                "the stop changed during legacy baseline recovery"
            );
            anyhow::ensure!(
                index_outside(scratch, &last.paths).await? == baseline,
                "the legacy index differs from the reconstructed clean baseline"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = validation {
            return Err(infra_stop(
                state,
                format!("legacy baseline recovery: {e:#}"),
            ));
        }
        let first = state.rebase_fixes.len() - stop_records(state).len();
        state.rebase_fixes[first].index_outside = Some(baseline);
        for record in &mut state.rebase_fixes[first..] {
            record.stop = Some(pick.clone());
        }
        state.event(
            "rebase",
            "verified and saved the legacy stop baseline from the original three-way merge",
        );
        state.save()?;
        lease
    } else {
        lease
    };
    if last.stop.is_none() {
        let pick = rebase_head(scratch)
            .await
            .context("legacy stop has no REBASE_HEAD")?;
        let first = state.rebase_fixes.len() - stop_records(state).len();
        for record in &mut state.rebase_fixes[first..] {
            record.stop = Some(pick.clone());
        }
        state.save()?;
    }
    let baseline = round_baseline(state, scratch, &last.paths).await?;
    let unmerged = match git::unmerged_paths(scratch).await {
        Ok(u) => u,
        Err(e) => return Err(infra_stop(state, format!("cannot read the index: {e:#}"))),
    };
    if let Some(p) = unmerged.iter().find(|p| !last.paths.contains(p)) {
        return Ok(Launch::Nothing(format!(
            "`{p}` is unmerged but was not part of the recorded conflict"
        )));
    }
    for p in &last.paths {
        match std::fs::symlink_metadata(scratch.join(p)) {
            Ok(m) if m.is_dir() => {
                return Ok(Launch::Nothing(format!("`{p}` is a directory")));
            }
            Ok(m) if m.is_file() => {
                if let Err(e) = std::fs::read(scratch.join(p)) {
                    return Err(infra_stop(state, format!("cannot read `{p}`: {e}")));
                }
                if has_markers(scratch, p, &["HEAD", "REBASE_HEAD"]).await? {
                    return Ok(Launch::Nothing(format!(
                        "`{p}` still holds conflict markers"
                    )));
                }
            }
            Ok(_) => {}
            // Resolved by deleting the file.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(infra_stop(state, format!("cannot inspect `{p}`: {e}"))),
        }
    }
    let entries = match status_entries(scratch).await {
        Ok(e) => e,
        Err(e) => return Err(infra_stop(state, format!("{e:#}"))),
    };
    // Whatever Git applied cleanly is staged and identical to the worktree;
    // anything else outside the conflict is an edit nobody asked for.
    let stray: Vec<&str> = entries
        .iter()
        .filter(|(_, p)| !last.paths.contains(p))
        .filter(|(xy, _)| xy == "??" || !xy.ends_with(' '))
        .map(|(_, p)| p.as_str())
        .collect();
    if !stray.is_empty() {
        return Ok(Launch::Nothing(format!(
            "changes outside the conflicted paths: {}",
            stray.join(", ")
        )));
    }
    match index_outside(scratch, &last.paths).await {
        Ok(now) if now == baseline => {}
        Ok(_) => {
            return Ok(Launch::Nothing(
                "the staged changes outside the conflicted paths changed".into(),
            ));
        }
        Err(e) => return Err(infra_stop(state, format!("{e:#}"))),
    }

    let set = |state: &mut RunState, step| {
        if let Some(r) = state.rebase_fixes.last_mut() {
            r.launcher = Some(step);
        }
        state.save()
    };
    // A previous pass may have staged the resolution before being interrupted.
    // Include later worktree edits to those recorded paths as well, while
    // leaving already-staged deletions (which have no path to add) alone.
    let to_stage: Vec<&str> = last
        .paths
        .iter()
        .filter(|p| {
            unmerged.contains(p)
                || entries
                    .iter()
                    .any(|(xy, path)| path == *p && !xy.ends_with(' '))
        })
        .map(String::as_str)
        .collect();
    if !to_stage.is_empty() {
        set(state, crate::run::LauncherStep::Staging)?;
        let mut args = vec!["--literal-pathspecs", "add", "-A", "--"];
        args.extend(to_stage);
        match mutate(scratch, &args, lease).await {
            Ok(o) if o.ok() => {}
            Ok(o) => {
                let said = if o.stderr.is_empty() {
                    o.stdout
                } else {
                    o.stderr
                };
                return Err(infra_stop(state, format!("staging was refused: {said}")));
            }
            Err(e) => return Err(infra_stop(state, format!("{e:#}"))),
        }
        match git::unmerged_paths(scratch).await {
            Ok(left) if left.is_empty() => {}
            Ok(left) => {
                return Err(infra_stop(
                    state,
                    format!("paths stayed unmerged after staging: {}", left.join(", ")),
                ));
            }
            Err(e) => return Err(infra_stop(state, format!("{e:#}"))),
        }
        // Running releases coordination only after all descendants stop.
        // Reacquire it and revalidate the stop before the next mutation.
        return Box::pin(launcher_stage(state, scratch, original, onto)).await;
    }
    set(state, crate::run::LauncherStep::Staged)?;
    let before = rebase_head(scratch).await;
    set(state, crate::run::LauncherStep::Continuing)?;
    let out = match mutate(scratch, &["rebase", "--continue"], lease).await {
        Ok(o) => o,
        Err(e) => return Err(infra_stop(state, format!("{e:#}"))),
    };
    if !out.ok() && git::rebase_in_progress(scratch).await {
        let again = git::unmerged_paths(scratch).await.unwrap_or_default();
        // A new conflict is the next stop point, not a failure; the same
        // stop with nothing unmerged is git unable to write.
        if again.is_empty() || rebase_head(scratch).await == before {
            let said = git::rebase_said(&if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            });
            return Err(infra_stop(state, format!("continue failed: {said}")));
        }
    }
    state.event(
        "rebase",
        format!(
            "launcher staged {} path(s) and continued the rebase",
            unmerged.len()
        ),
    );
    Ok(Launch::Advanced)
}

/// Judge a rebase that is no longer in progress by what git says.
async fn finish(
    state: &mut RunState,
    scratch: &Path,
    branch: &str,
    orig: &str,
    onto_sha: &str,
    touched: &[String],
    said: &str,
) -> Result<Rebased> {
    let repo = state.repo.clone();
    let spent = state.rebase_fixes.len();
    let cap = state.config.graph.review_rounds;
    let unmerged = git::unmerged_paths(scratch).await.unwrap_or_default();
    let head = git::rev_parse(scratch, "HEAD").await.unwrap_or_default();
    // Not only the paths seen unmerged at a round's start: one round may
    // continue through several commits, and a later commit's conflict never
    // shows up at a loop head. Everything the result changes relative to the
    // base is checked.
    let mut candidates: Vec<String> = touched.to_vec();
    if let Ok(changed) = git::git(scratch, &["diff", "--name-only", onto_sha, "HEAD"]).await {
        for p in changed.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !candidates.iter().any(|c| c == p) {
                candidates.push(p.to_owned());
            }
        }
    }
    let mut marked = Vec::new();
    for p in candidates {
        if has_markers(scratch, &p, &[orig, onto_sha]).await? {
            marked.push(p);
        }
    }

    // Skipping every conflicting commit leaves the branch equal to the base:
    // the ancestry test below passes and the branch's work is gone. Empty is
    // only acceptable when every original commit already has a patch twin on
    // the base.
    let emptied = head == onto_sha
        && git::cherry(&repo, onto_sha, orig)
            .await
            .map_or(true, |(unmatched, _)| !unmatched.is_empty());

    // A fixer that ran `git rebase --skip` on one of several commits lets the
    // rest apply and the rebase finish; `emptied` only sees the whole change
    // vanishing. Find the commits that are neither in the result nor already
    // on the base.
    let dropped = if unmerged.is_empty() && marked.is_empty() && !emptied && !head.is_empty() {
        dropped_commits(&repo, scratch, onto_sha, orig, &head).await
    } else {
        Ok(Vec::new())
    };

    let problem = if !unmerged.is_empty() {
        Some(("paths are still unmerged", unmerged))
    } else if !marked.is_empty() {
        Some(("conflict markers were left in the tree", marked))
    } else if emptied {
        Some((
            "the rebase ended with none of the branch's commits applied (all skipped)",
            touched.to_vec(),
        ))
    } else if head.is_empty() || !git::is_ancestor(&repo, onto_sha, &head).await {
        Some((
            "the rebase ended without the base in the result (abandoned or skipped)",
            touched.to_vec(),
        ))
    } else if let Err(error) = &dropped {
        Some((
            "the rebase record could not be verified",
            vec![format!("{error:#}")],
        ))
    } else if !dropped.as_ref().unwrap().is_empty() {
        Some((
            "the rebase dropped some of the branch's commits (skipped?)",
            dropped.unwrap(),
        ))
    } else {
        None
    };
    match problem {
        None => {
            git::worktree_remove(&repo, scratch).await.ok();
            state.event(
                "rebase",
                format!("{branch} rebased after {spent} fixer round(s)"),
            );
            state.save()?;
            Ok(Rebased::Applied)
        }
        Some((what, paths)) => {
            let why = reason(spent, cap, &paths, said, what);
            abandon(&repo, scratch, branch, orig).await;
            Ok(Rebased::Stopped(why))
        }
    }
}

/// Only a pick whose exec observed the previous HEAD could have been skipped.
/// Content checks distinguish a genuine loss from a change already upstream.
async fn dropped_commits(
    repo: &Path,
    scratch: &Path,
    onto_sha: &str,
    orig: &str,
    head: &str,
) -> Result<Vec<String>> {
    let record = git::rebase_record(scratch).await?;
    anyhow::ensure!(
        record.heads.last().map(String::as_str) == Some(head),
        "rebase final HEAD disagrees with record"
    );
    let mut previous = onto_sha;
    let mut lost = Vec::new();
    for (sha, picked_head) in record.picks.iter().zip(&record.heads) {
        if picked_head == previous
            && !absorbed_in_result(repo, sha, onto_sha, orig, head).await?
            && !already_in_result(repo, sha, orig, head).await?
        {
            lost.push(git::git(repo, &["show", "-s", "--format=%s", sha]).await?);
        }
        previous = picked_head;
    }
    Ok(lost)
}

/// Paths `sha` changes, NUL-separated from git so a quoted name is never
/// misread. Read failures propagate to the rebase verdict.
async fn commit_paths(repo: &Path, sha: &str) -> Result<Vec<String>> {
    git::git(
        repo,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "--root",
            "-z",
            sha,
        ],
    )
    .await
    .map(|o| {
        o.split('\0')
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    })
}

/// `mode type blob` of `path` at `rev`, or `None` when it does not exist
/// there. Unlike `rev-parse rev:path` this carries the file mode.
async fn entry(repo: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    let out = git::git(repo, &["--literal-pathspecs", "ls-tree", rev, "--", path]).await?;
    Ok(out
        .split('\t')
        .next()
        .filter(|e| !e.is_empty())
        .map(str::to_owned))
}

/// Is `sha`'s change represented in `head`?
///
/// First by merging: replaying `sha` onto `head` changes nothing when its
/// change is already there, in whatever company (extra upstream edits in the
/// same file included). Failing that, per touched path: `head` holds what
/// `sha` left there, or what the branch's final tip `orig` has there (a later
/// commit may have changed the path again). Mode is part of that comparison,
/// and paths come NUL-separated so a quoted name is never misread.
async fn already_in_result(repo: &Path, sha: &str, orig: &str, head: &str) -> Result<bool> {
    if replay_is_noop(repo, sha, head).await? {
        return Ok(true);
    }
    for p in &commit_paths(repo, sha).await? {
        let got = entry(repo, head, p).await?;
        if got != entry(repo, sha, p).await? && got != entry(repo, orig, p).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Does merging `sha` (against its parent) into `head` cleanly yield `head`'s
/// own tree? Conflicts and changes return false; read failures propagate.
async fn replay_is_noop(repo: &Path, sha: &str, head: &str) -> Result<bool> {
    let base = format!("{sha}^");
    let out = git::git_raw(
        repo,
        &[
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={base}"),
            head,
            sha,
        ],
    )
    .await?;
    anyhow::ensure!(
        out.code == Some(0) || out.code == Some(1),
        "merge-tree failed: {}",
        out.stderr
    );
    if !out.ok() {
        return Ok(false);
    }
    let merged = out
        .stdout
        .lines()
        .next()
        .context("merge-tree returned no tree")?
        .trim();
    Ok(merged == git::tree_of(repo, head).await?)
}

/// An absorbed change may be superseded by later branch commits, so replaying
/// it alone onto the result can conflict. Starting at `sha`, merge only those
/// later changes with the base and require every path `sha` touched to match
/// the result (including modes). Project all four trees onto just those paths
/// first: conflicts elsewhere cannot invalidate this commit's surviving effect.
/// A conflict on a candidate path or a failed lookup is not evidence of survival.
async fn absorbed_in_result(
    repo: &Path,
    sha: &str,
    onto: &str,
    orig: &str,
    head: &str,
) -> Result<bool> {
    if !replay_is_noop(repo, sha, onto).await? {
        return Ok(false);
    }
    let paths = commit_paths(repo, sha).await?;
    if paths.is_empty() {
        return Ok(false);
    }
    let base = path_tree(repo, sha, &paths).await?;
    let upstream = path_tree(repo, onto, &paths).await?;
    let branch = path_tree(repo, orig, &paths).await?;
    let result = path_tree(repo, head, &paths).await?;
    let out = git::git_raw(
        repo,
        &[
            "merge-tree",
            "--write-tree",
            &format!("--merge-base={base}"),
            &upstream,
            &branch,
        ],
    )
    .await?;
    anyhow::ensure!(
        out.code == Some(0) || out.code == Some(1),
        "merge-tree failed: {}",
        out.stderr
    );
    if !out.ok() {
        return Ok(false);
    }
    Ok(out
        .stdout
        .lines()
        .next()
        .context("merge-tree returned no tree")?
        == result)
}

/// A tree containing only the candidate paths, under stable synthetic names.
/// Preserve each entry's object and mode, and leave missing entries absent.
/// Flattening the names lets mktree handle nested and unusual paths without
/// an index or a working tree, and keeps unrelated paths out of the merge.
async fn path_tree(repo: &Path, rev: &str, paths: &[String]) -> Result<String> {
    use crate::proc::Quiet as _;
    use std::fmt::Write as _;
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;

    let mut entries = String::new();
    for (i, path) in paths.iter().enumerate() {
        let out = git::git(
            repo,
            &["--literal-pathspecs", "ls-tree", "-z", rev, "--", path],
        )
        .await?;
        if !out.is_empty() {
            let (entry, _) = out
                .split_once('\t')
                .context("ls-tree returned an invalid entry")?;
            writeln!(entries, "{entry}\t{i:016x}")?;
        }
    }
    #[cfg(test)]
    crate::web::request_cost::git();
    let mut child = tokio::process::Command::new("git")
        .args(["mktree"])
        .current_dir(repo)
        .quiet()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().context("mktree stdin missing")?;
    stdin.write_all(entries.as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    anyhow::ensure!(
        out.status.success(),
        "mktree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}

/// Give up: abort whatever is standing, drop the worktree and put the branch
/// ref back where it was, whatever a fixer did to it.
async fn abandon(repo: &Path, scratch: &Path, branch: &str, orig: &str) {
    git::rebase_abort(repo, scratch).await;
    let full = format!("refs/heads/{branch}");
    if git::rev_parse(repo, &full).await.ok().as_deref() != Some(orig) {
        git::git_raw(repo, &["update-ref", &full, orig]).await.ok();
    }
}

/// The reason a caller records and the conductor quotes: it says what was
/// tried, so nobody has to open the run directory to find out.
fn reason(spent: usize, cap: usize, paths: &[String], said: &str, what: &str) -> String {
    let shown: Vec<&str> = paths
        .iter()
        .take(PATHS_IN_REASON)
        .map(String::as_str)
        .collect();
    let mut list = shown.join(", ");
    if paths.len() > shown.len() {
        list.push_str(&format!(" and {} more", paths.len() - shown.len()));
    }
    if list.is_empty() {
        list.push_str("none recorded");
    }
    let mut s = format!(
        "conflict not resolved after {spent} of {cap} fixer round(s) ({what}); remaining \
         conflicted path(s): {list}"
    );
    let said = git::rebase_said(said);
    let said = said.trim();
    if !said.is_empty() {
        s.push_str("; git said: ");
        s.extend(said.chars().take(250));
    }
    s
}

/// Commit subjects in `range`, newest first, capped.
async fn subjects(worktree: &Path, range: &str) -> Vec<String> {
    let n = format!("-n{SUBJECTS}");
    git::git(worktree, &["log", "--format=%s", &n, range])
        .await
        .map(|o| o.lines().map(str::to_owned).collect())
        .unwrap_or_default()
}

/// A separator alone is ambiguous (for example a Setext heading). Allow
/// separators already present under the same heading on either side, while
/// rejecting new ones left by a resolution. Opening, closing and diff3 markers stand on their own.
async fn has_markers(worktree: &Path, path: &str, sides: &[&str]) -> Result<bool> {
    let Ok(text) = std::fs::read_to_string(worktree.join(path)) else {
        return Ok(false);
    };
    if text
        .lines()
        .any(|l| l.starts_with("<<<<<<<") || l.starts_with(">>>>>>>") || l.starts_with("|||||||"))
    {
        return Ok(true);
    }
    fn separators(text: &str) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        let mut previous = "";
        for line in text.lines() {
            if line == "=======" {
                *counts.entry(previous.to_owned()).or_default() += 1;
            }
            previous = line;
        }
        counts
    }
    let current = separators(&text);
    if current.is_empty() {
        return Ok(false);
    }
    let mut existing = std::collections::BTreeMap::<String, usize>::new();
    for side in sides {
        // A missing file is expected for add/add conflicts. Other failures
        // must not be interpreted as an empty baseline.
        let spec = format!("{side}:{path}");
        let exists = git::git_raw(worktree, &["cat-file", "-e", &spec]).await?;
        if exists.ok() {
            for (heading, count) in separators(&git::git(worktree, &["show", &spec]).await?) {
                let before = existing.entry(heading).or_default();
                *before = (*before).max(count);
            }
        } else {
            // Verify the tree itself remains readable before treating a
            // missing entry as an empty side.
            git::rev_parse(worktree, &format!("{side}^{{tree}}")).await?;
        }
    }
    Ok(current
        .into_iter()
        .any(|(heading, count)| count > existing.get(&heading).copied().unwrap_or(0)))
}

/// The conflicted regions of `paths`, marker lines included, truncated. This
/// only *shows* the conflict; nothing here decides how to resolve it.
fn hunks(worktree: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for p in paths {
        if out.len() >= HUNK_TOTAL {
            out.push_str("\n(more conflicted files omitted)\n");
            break;
        }
        out.push_str(&format!("=== {p} ===\n"));
        let Ok(text) = std::fs::read_to_string(worktree.join(p)) else {
            out.push_str("(not readable as text; use git to inspect it)\n");
            continue;
        };
        let mut file = String::new();
        let mut inside = false;
        for line in text.lines() {
            if line.starts_with("<<<<<<< ") {
                inside = true;
            }
            if inside {
                file.push_str(line);
                file.push('\n');
            }
            if line.starts_with(">>>>>>> ") {
                inside = false;
            }
        }
        if file.len() > HUNK_PER_FILE {
            file = file.chars().take(HUNK_PER_FILE).collect();
            file.push_str("\n(truncated)\n");
        }
        out.push_str(&file);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::Quiet as _;
    use std::path::PathBuf;

    fn sh(dir: &Path, args: &[&str]) {
        let o = std::process::Command::new("git")
            .quiet()
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    async fn record(d: &Path, picks: &[String], heads: &[String]) {
        for (name, values) in [("magi-rebase-picks", picks), ("magi-rebase-heads", heads)] {
            let path = git::git(d, &["rev-parse", "--git-path", name])
                .await
                .unwrap();
            std::fs::write(d.join(path), values.join("\n") + "\n").unwrap();
        }
    }

    #[tokio::test]
    async fn a_lost_mode_change_is_not_already_in_the_result() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        sh(d, &["config", "core.fileMode", "true"]);
        std::fs::write(d.join("script.sh"), "echo\n").unwrap();
        sh(d, &["add", "-A"]);
        sh(d, &["commit", "-q", "-m", "base"]);
        sh(d, &["update-index", "--chmod=+x", "script.sh"]);
        sh(d, &["commit", "-q", "-m", "chmod"]);
        let sha = git::git(d, &["rev-parse", "HEAD"]).await.unwrap();
        let base = git::git(d, &["rev-parse", "HEAD~1"]).await.unwrap();
        // The result is the base: same blob, lost mode.
        assert!(!already_in_result(d, &sha, &sha, &base).await.unwrap());
        assert!(already_in_result(d, &sha, &sha, &sha).await.unwrap());
    }

    async fn commit(d: &Path, msg: &str) -> String {
        sh(d, &["add", "-A"]);
        sh(d, &["commit", "-q", "-m", msg]);
        git::git(d, &["rev-parse", "HEAD"]).await.unwrap()
    }

    #[tokio::test]
    async fn candidate_path_trees_preserve_modes_deletions_and_unusual_names() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        sh(d, &["config", "core.fileMode", "true"]);
        let path = if cfg!(windows) {
            "nested/日本語 [literal].txt"
        } else {
            "nested/日本語\tline\n.txt"
        };
        std::fs::create_dir(d.join("nested")).unwrap();
        std::fs::write(d.join(path), "contents\n").unwrap();
        std::fs::write(d.join("g.txt"), "old\n").unwrap();
        let base = commit(d, "base").await;
        let paths = vec![path.to_owned()];
        let tree = path_tree(d, &base, &paths).await.unwrap();
        std::fs::write(d.join("g.txt"), "unrelated\n").unwrap();
        let other = commit(d, "unrelated path").await;
        assert_eq!(path_tree(d, &other, &paths).await.unwrap(), tree);
        sh(d, &["update-index", "--chmod=+x", path]);
        sh(d, &["commit", "-q", "-m", "chmod"]);
        assert_ne!(path_tree(d, "HEAD", &paths).await.unwrap(), tree);
        std::fs::remove_file(d.join(path)).unwrap();
        let removed = commit(d, "delete").await;
        let absent = path_tree(d, &removed, &paths).await.unwrap();
        assert_ne!(absent, tree);
        assert_eq!(absent, path_tree(d, &removed, &[]).await.unwrap());
        assert!(path_tree(d, "not-a-revision", &paths).await.is_err());
    }

    #[tokio::test]
    async fn a_change_inside_a_larger_upstream_edit_is_in_the_result() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        let body = "old\n1\n2\n3\n4\n5\n6\n7\n8\n9\nend\n";
        std::fs::write(d.join("f.txt"), body).unwrap();
        commit(d, "base").await;
        std::fs::write(d.join("f.txt"), body.replacen("old", "new", 1)).unwrap();
        let c = commit(d, "c1").await;
        sh(d, &["checkout", "-q", "-b", "up", "HEAD~1"]);
        let up = body.replacen("old", "new", 1).replace("end", "end plus");
        std::fs::write(d.join("f.txt"), up).unwrap();
        let head = commit(d, "upstream").await;
        assert!(already_in_result(d, &c, &c, &head).await.unwrap());
    }

    #[tokio::test]
    async fn a_dropped_commit_on_a_non_ascii_path_is_still_noticed() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        let base = commit(d, "base").await;
        std::fs::write(d.join("日本語.txt"), "x\n").unwrap();
        let c = commit(d, "c").await;
        assert!(!already_in_result(d, &c, &c, &base).await.unwrap());
    }

    async fn commit_dated(d: &Path, msg: &str) -> String {
        sh(d, &["add", "-A"]);
        let st = std::process::Command::new("git")
            .quiet()
            .current_dir(d)
            .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00+0000")
            .env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00+0000")
            .args(["commit", "-q", "-m", msg])
            .status()
            .unwrap();
        assert!(st.success());
        git::git(d, &["rev-parse", "HEAD"]).await.unwrap()
    }

    #[tokio::test]
    async fn a_recorded_resolved_pick_after_an_absorbed_one_is_not_lost() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        std::fs::write(d.join("z.txt"), "z\n").unwrap();
        let base = commit(d, "base").await;
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        commit_dated(d, "same").await;
        std::fs::write(d.join("b.txt"), "b\n").unwrap();
        let orig = commit_dated(d, "same").await;
        // Upstream folds the first change into a differently-shaped commit
        // and adds a conflicting b.txt.
        sh(d, &["checkout", "-q", "-b", "up", &base]);
        std::fs::write(d.join("a.txt"), "a\n").unwrap();
        std::fs::write(d.join("y.txt"), "y\n").unwrap();
        std::fs::write(d.join("b.txt"), "other\n").unwrap();
        let onto = commit(d, "upstream").await;
        // The rebase result: first commit omitted as empty, second resolved.
        std::fs::write(d.join("b.txt"), "other\nb\n").unwrap();
        let head = commit_dated(d, "same").await;
        record(
            d,
            &[
                git::rev_parse(d, &format!("{orig}^")).await.unwrap(),
                orig.clone(),
            ],
            &[onto.clone(), head.clone()],
        )
        .await;
        assert!(
            dropped_commits(d, d, &onto, &orig, &head)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_skipped_commit_is_not_masked_by_a_surviving_one_in_the_same_file() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        let body = "first\n1\n2\n3\n4\n5\n6\n7\n8\n9\nlast\n";
        std::fs::write(d.join("f.txt"), body).unwrap();
        let base = commit(d, "base").await;
        std::fs::write(d.join("f.txt"), body.replacen("first", "mine", 1)).unwrap();
        commit_dated(d, "same").await;
        let two = body
            .replacen("first", "mine", 1)
            .replacen("last", "tail", 1);
        std::fs::write(d.join("f.txt"), &two).unwrap();
        let orig = commit_dated(d, "same").await;
        sh(d, &["checkout", "-q", "-b", "up", &base]);
        std::fs::write(d.join("f.txt"), body.replacen("first", "theirs", 1)).unwrap();
        let onto = commit(d, "upstream").await;
        // First commit skipped, second applied untouched.
        std::fs::write(
            d.join("f.txt"),
            body.replacen("first", "theirs", 1)
                .replacen("last", "tail", 1),
        )
        .unwrap();
        let head = commit_dated(d, "same").await;
        record(
            d,
            &[
                git::rev_parse(d, &format!("{orig}^")).await.unwrap(),
                orig.clone(),
            ],
            &[onto.clone(), head.clone()],
        )
        .await;
        assert_eq!(
            dropped_commits(d, d, &onto, &orig, &head).await.unwrap(),
            vec!["same"]
        );
    }

    #[tokio::test]
    async fn git_records_a_skipped_middle_pick_after_an_applied_pick() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        std::fs::write(d.join("f"), "base\n").unwrap();
        let base = commit(d, "base").await;
        sh(d, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(d.join("absorbed"), "upstream too\n").unwrap();
        commit(d, "already upstream").await;
        std::fs::write(d.join("a"), "first\n").unwrap();
        let first = commit(d, "first").await;
        std::fs::write(d.join("f"), "middle\n").unwrap();
        let middle = commit(d, "lost middle").await;
        std::fs::write(d.join("z"), "last\n").unwrap();
        let orig = commit(d, "last").await;
        sh(d, &["checkout", "-q", "main"]);
        std::fs::write(d.join("absorbed"), "upstream too\n").unwrap();
        commit(d, "patch twin").await;
        std::fs::write(d.join("f"), "upstream\n").unwrap();
        let onto = commit(d, "upstream").await;
        let scratch = d.join("scratch");
        let git::RebaseStart::Conflicted(why) = git::rebase_start(d, &scratch, "topic", &onto)
            .await
            .unwrap()
        else {
            panic!("expected a conflict");
        };
        assert!(!why.contains("Executing:"), "{why}");
        let skipped = git::git_raw(&scratch, &["rebase", "--skip"]).await.unwrap();
        assert!(skipped.ok(), "{}", skipped.stderr);
        let head = git::rev_parse(&scratch, "HEAD").await.unwrap();
        let record = git::rebase_record(&scratch).await.unwrap();
        assert_eq!(record.picks, vec![first, middle, orig.clone()]);
        assert_eq!(record.heads[0], record.heads[1]);
        assert_ne!(record.heads[1], record.heads[2]);
        assert_eq!(
            dropped_commits(d, &scratch, &onto, &orig, &head)
                .await
                .unwrap(),
            vec!["lost middle"]
        );
        git::rebase_abort(d, &scratch).await;
        assert!(git::is_ancestor(d, &base, &head).await);
    }

    #[test]
    fn recording_diagnostics_do_not_hide_other_git_output() {
        assert_eq!(
            git::rebase_said("before\nExecuting: record\nafter\n"),
            "before\nafter"
        );
    }

    // Exercise the actual rebase and finish path. C0 conflicts in another
    // file; C1 and C2 successively change the first line of f.txt. Upstream
    // also changes a distant line, so neither C1 nor the original tip has
    // the result's whole-file snapshot.
    async fn rebase_successive_edits(
        absorbed: bool,
        extra_hunk: bool,
        unrelated_conflict: bool,
    ) -> Rebased {
        rebase_successive_edits_record(absorbed, extra_hunk, unrelated_conflict, None).await
    }

    async fn rebase_successive_edits_record(
        absorbed: bool,
        extra_hunk: bool,
        unrelated_conflict: bool,
        damage: Option<&str>,
    ) -> Rebased {
        crate::run::pin_test_home();
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        // Keep Git checkouts consistent with this fixture's LF byte strings,
        // even when the host enables automatic CRLF conversion.
        sh(d, &["config", "core.autocrlf", "false"]);
        let body = "old\n1\n2\n3\n4\n5\n6\n7\n8\n9\nend\n";
        std::fs::write(d.join("f.txt"), body).unwrap();
        std::fs::write(d.join("conflict.txt"), "old\n").unwrap();
        if unrelated_conflict {
            std::fs::write(d.join("g.txt"), "old\n").unwrap();
        }
        let base = commit(d, "base").await;
        sh(d, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(d.join("conflict.txt"), "branch\n").unwrap();
        commit(d, "c0").await;
        let mut middle = body.replacen("old", "new", 1);
        if extra_hunk {
            middle = middle.replace("\n5\n", "\nmissing\n");
        }
        std::fs::write(d.join("f.txt"), &middle).unwrap();
        let c1 = commit(d, "c1").await;
        std::fs::write(d.join("f.txt"), middle.replacen("new", "newer", 1)).unwrap();
        if unrelated_conflict {
            std::fs::write(d.join("g.txt"), "branch\n").unwrap();
        }
        let orig = commit(d, "c2").await;
        sh(d, &["checkout", "-q", "main"]);
        std::fs::write(d.join("conflict.txt"), "branch plus upstream\n").unwrap();
        let mut upstream = body
            .replacen("old", if absorbed { "new" } else { "theirs" }, 1)
            .replace("end", "upstream tail");
        if extra_hunk {
            upstream = upstream.replace("\n5\n", "\nupstream middle\n");
        }
        std::fs::write(d.join("f.txt"), &upstream).unwrap();
        if unrelated_conflict {
            std::fs::write(d.join("g.txt"), "upstream\n").unwrap();
        }
        let onto = commit(d, "upstream composite").await;
        // The composite has no patch-id twin for C1.
        assert!(git::cherry(d, &onto, &orig).await.unwrap().0.contains(&c1));
        let scratch = d.join("scratch");
        assert!(matches!(
            git::rebase_start(d, &scratch, "topic", &onto)
                .await
                .unwrap(),
            git::RebaseStart::Conflicted(_)
        ));
        std::fs::write(
            scratch.join("conflict.txt"),
            "branch plus upstream resolved\n",
        )
        .unwrap();
        sh(&scratch, &["add", "conflict.txt"]);
        let continued = git::git_raw(
            &scratch,
            &["-c", "core.editor=true", "rebase", "--continue"],
        )
        .await
        .unwrap();
        if absorbed && !extra_hunk {
            // C1 becomes empty automatically. C2 may conflict on a path C1
            // never touched, while its f.txt change has applied correctly.
            if unrelated_conflict {
                assert!(!continued.ok());
                assert_eq!(git::unmerged_paths(&scratch).await.unwrap(), vec!["g.txt"]);
                assert_eq!(git::rev_parse(&scratch, "REBASE_HEAD").await.unwrap(), orig);
                std::fs::write(scratch.join("g.txt"), "branch plus upstream resolved\n").unwrap();
                sh(&scratch, &["add", "g.txt"]);
                let resolved = git::git_raw(&scratch, &["rebase", "--continue"])
                    .await
                    .unwrap();
                assert!(resolved.ok(), "{}", resolved.stderr);
                // A whole-tree merge would reject the correctly rebased C1
                // solely because of the unrelated g.txt conflict.
                let full = git::git_raw(
                    d,
                    &[
                        "merge-tree",
                        "--write-tree",
                        &format!("--merge-base={c1}"),
                        &onto,
                        &orig,
                    ],
                )
                .await
                .unwrap();
                assert_eq!(full.code, Some(1));
            } else {
                assert!(continued.ok(), "{}", continued.stderr);
            }
        } else {
            assert!(!continued.ok());
            assert_eq!(git::unmerged_paths(&scratch).await.unwrap(), vec!["f.txt"]);
            let skipped = git::git_raw(&scratch, &["rebase", "--skip"]).await.unwrap();
            if !absorbed {
                // C2 now conflicts because its predecessor was really skipped.
                assert!(!skipped.ok());
                std::fs::write(
                    scratch.join("f.txt"),
                    upstream.replacen("theirs", "newer", 1),
                )
                .unwrap();
                sh(&scratch, &["add", "f.txt"]);
                let resolved = git::git_raw(
                    &scratch,
                    &["-c", "core.editor=true", "rebase", "--continue"],
                )
                .await
                .unwrap();
                assert!(resolved.ok(), "{}", resolved.stderr);
            } else {
                assert!(skipped.ok(), "{}", skipped.stderr);
            }
        }
        assert!(!git::rebase_in_progress(&scratch).await);
        assert_eq!(
            std::fs::read_to_string(scratch.join("f.txt")).unwrap(),
            upstream.replacen(if absorbed { "new" } else { "theirs" }, "newer", 1)
        );
        let head = git::rev_parse(&scratch, "HEAD").await.unwrap();
        // The old final-tree-only check cannot account for C1 in any case.
        assert!(!already_in_result(d, &c1, &orig, &head).await.unwrap());
        let mut state = RunState::new(
            d.to_path_buf(),
            "main".into(),
            base,
            "rebase".into(),
            crate::config::Config::default(),
        );
        if let Some(damage) = damage {
            let path = git::git(&scratch, &["rev-parse", "--git-path", "magi-rebase-heads"])
                .await
                .unwrap();
            match damage {
                "missing" => std::fs::remove_file(&path).unwrap(),
                "count" => std::fs::write(&path, "").unwrap(),
                "final" => {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let mut heads: Vec<_> = text.lines().map(str::to_owned).collect();
                    *heads.last_mut().unwrap() = onto.clone();
                    std::fs::write(&path, heads.join("\n") + "\n").unwrap();
                }
                "invalid" => std::fs::write(&path, "invalid\n").unwrap(),
                _ => unreachable!(),
            }
        }
        let outcome = finish(
            &mut state,
            &scratch,
            "topic",
            &orig,
            &onto,
            &["conflict.txt".into()],
            "",
        )
        .await
        .unwrap();
        std::fs::remove_dir_all(state.dir()).ok();
        outcome
    }

    #[tokio::test]
    async fn missing_or_inconsistent_rebase_records_stop_the_rebase() {
        for damage in ["missing", "count", "final", "invalid"] {
            let outcome = rebase_successive_edits_record(true, false, false, Some(damage)).await;
            assert!(
                matches!(outcome, Rebased::Stopped(why) if why.contains("record could not be verified")),
                "{damage}"
            );
        }
    }

    #[tokio::test]
    async fn an_absorbed_middle_commit_superseded_by_a_later_edit_is_applied() {
        assert_eq!(
            rebase_successive_edits(true, false, false).await,
            Rebased::Applied
        );
    }

    #[tokio::test]
    async fn an_unrelated_later_conflict_does_not_hide_an_absorbed_commit() {
        assert_eq!(
            rebase_successive_edits(true, false, true).await,
            Rebased::Applied
        );
    }

    #[tokio::test]
    async fn a_skipped_middle_commit_superseded_by_a_later_edit_is_dropped() {
        let Rebased::Stopped(why) = rebase_successive_edits(false, false, false).await else {
            panic!("a skipped middle commit must stop the rebase");
        };
        assert!(why.contains("the rebase dropped some of the branch's commits (skipped?)"));
        assert!(why.contains("c1"));
    }

    #[tokio::test]
    async fn a_partially_absorbed_middle_commit_with_a_skipped_hunk_is_dropped() {
        let Rebased::Stopped(why) = rebase_successive_edits(true, true, false).await else {
            panic!("an unabsorbed hunk must stop the rebase");
        };
        assert!(why.contains("the rebase dropped some of the branch's commits (skipped?)"));
        assert!(why.contains("c1"));
    }

    #[tokio::test]
    async fn an_absorbed_commit_erased_by_the_fixer_is_dropped() {
        crate::run::pin_test_home();
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        // This fixture controls each resolution itself; inherited rerere can
        // interfere with that sequence (including MERGE_RR lock contention).
        sh(d, &["config", "rerere.enabled", "false"]);
        let body =
            "old\n1\n2\n3\n4\n5\n6\n7\n8\n9\nlater\n10\n11\n12\n13\n14\n15\n16\n17\n18\nend\n";
        std::fs::write(d.join("f.txt"), body).unwrap();
        std::fs::write(d.join("conflict.txt"), "old\n").unwrap();
        let base = commit(d, "base").await;
        sh(d, &["checkout", "-q", "-b", "topic"]);
        std::fs::write(d.join("conflict.txt"), "branch\n").unwrap();
        commit(d, "c0").await;
        std::fs::write(d.join("f.txt"), body.replacen("old", "new", 1)).unwrap();
        let c1 = commit(d, "c1").await;
        std::fs::write(
            d.join("f.txt"),
            body.replacen("old", "new", 1)
                .replace("later", "branch tail"),
        )
        .unwrap();
        let orig = commit(d, "c2").await;
        sh(d, &["checkout", "-q", "main"]);
        std::fs::write(d.join("conflict.txt"), "upstream\n").unwrap();
        let upstream = body
            .replacen("old", "new", 1)
            .replace("end", "upstream tail");
        std::fs::write(d.join("f.txt"), &upstream).unwrap();
        let onto = commit(d, "upstream composite").await;
        let scratch = d.join("scratch");
        assert!(matches!(
            git::rebase_start(d, &scratch, "topic", &onto)
                .await
                .unwrap(),
            git::RebaseStart::Conflicted(_)
        ));
        // While resolving C0, the fixer erases C1's absorbed first-line edit.
        std::fs::write(scratch.join("conflict.txt"), "resolved\n").unwrap();
        std::fs::write(scratch.join("f.txt"), upstream.replacen("new", "theirs", 1)).unwrap();
        sh(&scratch, &["add", "conflict.txt", "f.txt"]);
        let continued = git::git_raw(&scratch, &["rebase", "--continue"])
            .await
            .unwrap();
        assert_eq!(
            continued.code,
            Some(1),
            "{}\n{}",
            continued.stdout,
            continued.stderr
        );
        assert_eq!(git::rev_parse(&scratch, "REBASE_HEAD").await.unwrap(), c1);
        let skipped = git::git_raw(&scratch, &["rebase", "--skip"]).await.unwrap();
        assert!(skipped.ok(), "{}", skipped.stderr);
        let head = git::rev_parse(&scratch, "HEAD").await.unwrap();
        assert!(replay_is_noop(d, &c1, &onto).await.unwrap());
        // The later branch commit edits a different hunk, so the clean merge
        // still requires "new" on line one. The final result has "theirs".
        let expected = git::git_raw(
            d,
            &[
                "merge-tree",
                "--write-tree",
                &format!("--merge-base={c1}"),
                &onto,
                &orig,
            ],
        )
        .await
        .unwrap();
        assert!(expected.ok(), "{}", expected.stderr);
        assert!(
            !absorbed_in_result(d, &c1, &onto, &orig, &head)
                .await
                .unwrap()
        );
        assert_eq!(
            dropped_commits(d, &scratch, &onto, &orig, &head)
                .await
                .unwrap(),
            vec!["c1"]
        );
        let mut state = RunState::new(
            d.to_path_buf(),
            "main".into(),
            base,
            "rebase".into(),
            crate::config::Config::default(),
        );
        let outcome = finish(
            &mut state,
            &scratch,
            "topic",
            &orig,
            &onto,
            &["conflict.txt".into()],
            "",
        )
        .await
        .unwrap();
        assert!(
            matches!(outcome, Rebased::Stopped(why) if why.contains("the rebase dropped some of the branch's commits (skipped?)") && why.contains("c1"))
        );
        std::fs::remove_dir_all(state.dir()).ok();
    }

    #[tokio::test]
    async fn a_partially_absorbed_commit_missing_an_unrelated_hunk_is_dropped() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.name", "t"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        std::fs::write(d.join("c0.txt"), "base c0\n").unwrap();
        let body = "old\n1\n2\n3\n4\n5\n6\n7\n8\n9\nlast\n";
        std::fs::write(d.join("f.txt"), body).unwrap();
        std::fs::write(d.join("extra.txt"), "initial\n").unwrap();
        let base = commit(d, "base").await;

        // Branch commits: C0, C1 (touches f.txt AND extra.txt), C2
        std::fs::write(d.join("c0.txt"), "branch c0\n").unwrap();
        commit_dated(d, "c0").await;
        std::fs::write(d.join("f.txt"), body.replacen("old", "new", 1)).unwrap();
        std::fs::write(d.join("extra.txt"), "c1 edit\n").unwrap();
        commit_dated(d, "c1").await;
        std::fs::write(d.join("f.txt"), body.replacen("old", "newer", 1)).unwrap();
        let orig = commit_dated(d, "c2").await;

        // Upstream absorbed C1's f.txt edit, but NOT extra.txt
        sh(d, &["checkout", "-q", "-b", "up", &base]);
        std::fs::write(d.join("c0.txt"), "upstream c0\n").unwrap();
        let up = body.replacen("old", "new", 1).replace("last", "tail");
        std::fs::write(d.join("f.txt"), &up).unwrap();
        let onto = commit(d, "upstream").await;

        // Result: C0 resolved, C1 was skipped or dropped despite extra.txt missing, C2 applied
        std::fs::write(d.join("c0.txt"), "resolved c0\n").unwrap();
        commit_dated(d, "c0").await;
        let res = up.replacen("new", "newer", 1);
        std::fs::write(d.join("f.txt"), &res).unwrap();
        // extra.txt remains "initial\n"
        let head = commit_dated(d, "c2").await;

        let c0 = git::rev_parse(d, &format!("{orig}~2")).await.unwrap();
        let c1 = git::rev_parse(d, &format!("{orig}^")).await.unwrap();
        let resolved = git::rev_parse(d, &format!("{head}^")).await.unwrap();
        record(
            d,
            &[c0, c1, orig.clone()],
            &[resolved.clone(), resolved, head.clone()],
        )
        .await;
        assert_eq!(
            dropped_commits(d, d, &onto, &orig, &head).await.unwrap(),
            vec!["c1"]
        );
    }

    // ---- launcher-owned staging -------------------------------------------

    const UPSTREAM: &str = "a\nupstream test\nz\n";
    const TOPIC: &str = "a\ntopic test\nz\n";
    const BOTH: &str = "a\nupstream test\ntopic test\nz\n";

    struct Fixture {
        _dir: tempfile::TempDir,
        scratch: PathBuf,
        state: RunState,
    }

    impl Fixture {
        fn read(&self, name: &str) -> String {
            std::fs::read_to_string(self.scratch.join(name)).unwrap()
        }
    }

    /// `topic` has two commits on `main`'s fork point: the first conflicts on
    /// `f.txt`, the second adds `g.txt`. The rebase stands on the first and
    /// one fixer round is recorded, as `rebase_with_fixer` would have.
    async fn fixture(stop_recorded: bool) -> Fixture {
        fixture_with(stop_recorded, "f.txt", 2).await
    }

    async fn fixture_with(stop_recorded: bool, conflict: &str, commits: usize) -> Fixture {
        fixture_stops(stop_recorded, conflict, commits, false).await
    }

    async fn fixture_stops(
        stop_recorded: bool,
        conflict: &str,
        commits: usize,
        second: bool,
    ) -> Fixture {
        crate::run::pin_test_home();
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("repo");
        std::fs::create_dir(&d).unwrap();
        sh(&d, &["init", "-q", "-b", "main"]);
        sh(&d, &["config", "user.name", "t"]);
        sh(&d, &["config", "user.email", "t@example.com"]);
        sh(&d, &["config", "core.autocrlf", "false"]);
        std::fs::write(d.join(conflict), "a\nbase\nz\n").unwrap();
        std::fs::write(d.join("automatic"), "base\n").unwrap();
        sh(&d, &["add", "."]);
        sh(&d, &["commit", "-qm", "base"]);
        let base = git::rev_parse(&d, "HEAD").await.unwrap();
        sh(&d, &["checkout", "-qb", "topic"]);
        std::fs::write(d.join(conflict), TOPIC).unwrap();
        std::fs::write(d.join("README.md"), "Heading\n=======\n").unwrap();
        sh(&d, &["add", "README.md"]);
        std::fs::write(d.join("automatic"), "topic applied cleanly\n").unwrap();
        sh(&d, &["commit", "-qam", "topic test"]);
        std::fs::write(d.join("g.txt"), "g\n").unwrap();
        sh(&d, &["add", "g.txt"]);
        sh(&d, &["commit", "-qm", "topic g"]);
        for n in 2..commits {
            std::fs::write(d.join(format!("commit-{n}")), format!("{n}\n")).unwrap();
            sh(&d, &["add", "."]);
            sh(&d, &["commit", "-qm", &format!("topic {n}")]);
        }
        sh(&d, &["checkout", "-q", "main"]);
        std::fs::write(d.join(conflict), UPSTREAM).unwrap();
        if second {
            std::fs::write(d.join("g.txt"), "upstream g\n").unwrap();
            sh(&d, &["add", "g.txt"]);
        }
        sh(&d, &["commit", "-qam", "upstream test"]);
        let scratch = t.path().join("scratch");
        let started = git::rebase_start(&d, &scratch, "topic", "main")
            .await
            .unwrap();
        assert!(matches!(started, git::RebaseStart::Conflicted(_)));
        let mut state = RunState::new(
            d.clone(),
            "main".into(),
            base,
            "rebase".into(),
            crate::config::Config::default(),
        );
        let paths = git::unmerged_paths(&scratch).await.unwrap();
        assert_eq!(paths, vec![conflict]);
        state.rebase_fixes.push(RebaseFixRecord {
            agent: "mock".into(),
            index_outside: index_outside(&scratch, &paths).await.ok(),
            stop: if stop_recorded {
                rebase_head(&scratch).await
            } else {
                None
            },
            paths,
            from: None,
            finished: false,
            error: None,
            launcher: None,
            infra: None,
        });
        Fixture {
            _dir: t,
            scratch,
            state,
        }
    }

    async fn stage(f: &mut Fixture) -> Result<Launch> {
        let original = git::rev_parse(&f.state.repo, "topic").await?;
        let onto = git::rev_parse(&f.state.repo, "main").await?;
        launcher_stage(&mut f.state, &f.scratch, &original, &onto).await
    }

    async fn topic_subjects(f: &Fixture) -> Vec<String> {
        subjects(&f.scratch, "main..HEAD").await
    }

    #[tokio::test]
    async fn resolved_contents_with_an_unmerged_index_are_staged_and_continued() {
        let mut f = fixture(true).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(out, Launch::Advanced));
        assert!(!git::rebase_in_progress(&f.scratch).await);
        // Both sides' tests survive and the later commit was replayed too.
        assert_eq!(f.read("f.txt"), BOTH);
        assert_eq!(f.read("g.txt"), "g\n");
        assert_eq!(topic_subjects(&f).await, vec!["topic g", "topic test"]);
        let rec = git::rebase_record(&f.scratch).await.unwrap();
        assert_eq!(rec.picks.len(), 2);
        assert_eq!(f.state.rebase_fixes.len(), 1, "no further round was spent");
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn a_pre_staging_record_is_matched_by_its_repeated_conflict_set() {
        let mut f = fixture(false).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        let again = f.state.rebase_fixes[0].clone();
        f.state.rebase_fixes.push(again);
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(out, Launch::Advanced));
        assert!(!git::rebase_in_progress(&f.scratch).await);

        // Records that disagree about the conflict identify nothing.
        let mut f = fixture(false).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        let mut other = f.state.rebase_fixes[0].clone();
        other.paths = vec!["elsewhere.txt".into()];
        f.state.rebase_fixes.insert(0, other);
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(out, Launch::Nothing(_)));
        assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["f.txt"]);
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn remaining_markers_stage_nothing() {
        let mut f = fixture(true).await;
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(&out, Launch::Nothing(why) if why.contains("markers")));
        assert!(git::rebase_in_progress(&f.scratch).await);
        assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["f.txt"]);
        assert!(f.state.resource_block.is_none());
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn partial_markers_are_rejected_even_after_staging() {
        for marker in ["<<<<<<< HEAD", "=======", ">>>>>>> topic", "||||||| base"] {
            for staged in [false, true] {
                let mut f = fixture(true).await;
                std::fs::write(f.scratch.join("f.txt"), format!("{BOTH}{marker}\n")).unwrap();
                if staged {
                    sh(&f.scratch, &["add", "f.txt"]);
                }
                assert!(
                    has_markers(&f.scratch, "f.txt", &["HEAD", "REBASE_HEAD"])
                        .await
                        .unwrap()
                );
                let out = stage(&mut f).await.unwrap();
                assert!(matches!(&out, Launch::Nothing(why) if why.contains("markers")));
                assert!(git::rebase_in_progress(&f.scratch).await);
                std::fs::remove_dir_all(f.state.dir()).ok();
            }
        }
    }

    #[tokio::test]
    async fn existing_markdown_headings_are_not_conflict_markers() {
        let f = fixture(true).await;
        assert!(
            !has_markers(&f.scratch, "README.md", &["HEAD", "REBASE_HEAD"])
                .await
                .unwrap()
        );
        std::fs::write(
            f.scratch.join("README.md"),
            "Heading\n=======\nextra separator\n=======\n",
        )
        .unwrap();
        assert!(
            has_markers(&f.scratch, "README.md", &["HEAD", "REBASE_HEAD"])
                .await
                .unwrap()
        );
        std::fs::write(f.scratch.join("README.md"), "resolved code\n=======\n").unwrap();
        assert!(
            has_markers(&f.scratch, "README.md", &["HEAD", "REBASE_HEAD"])
                .await
                .unwrap()
        );
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn retries_keep_the_first_index_baseline() {
        let mut f = fixture(true).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        std::fs::write(f.scratch.join("automatic"), "unrelated staged edit\n").unwrap();
        sh(&f.scratch, &["add", "automatic"]);
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(&out, Launch::Nothing(why) if why.contains("staged changes")));
        let paths = f.state.rebase_fixes[0].paths.clone();
        let baseline = round_baseline(&mut f.state, &f.scratch, &paths)
            .await
            .unwrap();
        assert_eq!(
            Some(&baseline),
            f.state.rebase_fixes[0].index_outside.as_ref()
        );
        assert_ne!(baseline, index_outside(&f.scratch, &paths).await.unwrap());
        let mut retry = f.state.rebase_fixes[0].clone();
        // Even a polluted later record cannot replace the first baseline.
        retry.index_outside = index_outside(&f.scratch, &paths).await.ok();
        f.state.rebase_fixes.push(retry);
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(&out, Launch::Nothing(why) if why.contains("staged changes")));
        assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["f.txt"]);
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn legacy_records_without_a_baseline_block_without_adopting_the_index() {
        let mut f = fixture(false).await;
        f.state.rebase_fixes[0].index_outside = None;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        std::fs::write(f.scratch.join("automatic"), "unrelated staged edit\n").unwrap();
        sh(&f.scratch, &["add", "automatic"]);
        let error = stage(&mut f).await.unwrap_err();
        assert!(error.is::<crate::repo_lock::ResourceBlocked>());
        assert!(error.to_string().contains("baseline"));
        assert_eq!(f.state.rebase_fixes.len(), 1);
        assert!(f.state.rebase_fixes[0].index_outside.is_none());
        assert_eq!(f.read("f.txt"), BOTH);
        assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["f.txt"]);
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn legacy_baseline_is_reconstructed_and_saved_without_another_fixer() {
        for staged in [false, true] {
            let mut f = fixture_with(false, "f.txt", 16).await;
            f.state.rebase_fixes[0].index_outside = None;
            let again = f.state.rebase_fixes[0].clone();
            f.state.rebase_fixes.push(again);
            f.state.config.graph.review_rounds = 1;
            std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
            if staged {
                sh(&f.scratch, &["add", "f.txt"]);
            }
            let original = git::rev_parse(&f.state.repo, "topic").await.unwrap();
            assert!(matches!(stage(&mut f).await.unwrap(), Launch::Advanced));
            assert_eq!(f.state.rebase_fixes.len(), 2);
            assert!(f.state.rebase_fixes[0].index_outside.is_some());
            assert!(f.state.rebase_fixes.iter().all(|r| r.stop.is_some()));
            let saved = RunState::load(&f.state.id).unwrap();
            assert_eq!(
                saved.rebase_fixes[0].index_outside,
                f.state.rebase_fixes[0].index_outside
            );
            assert_eq!(f.read("f.txt"), BOTH);
            assert_eq!(topic_subjects(&f).await.len(), 16);
            assert_eq!(f.read("automatic"), "topic applied cleanly\n");
            let onto = git::rev_parse(&f.state.repo, "main").await.unwrap();
            assert_eq!(
                finish(
                    &mut f.state,
                    &f.scratch,
                    "topic",
                    &original,
                    &onto,
                    &["f.txt".into()],
                    ""
                )
                .await
                .unwrap(),
                Rebased::Applied
            );
            std::fs::remove_dir_all(f.state.dir()).ok();
        }
    }

    #[tokio::test]
    async fn successful_progression_gets_a_budget_for_the_next_conflicting_commit() {
        let mut f = fixture_stops(true, "f.txt", 2, true).await;
        f.state.config.graph.review_rounds = 1;
        assert_eq!(stop_rounds(&f.state, &f.scratch).await, 1);
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        assert!(matches!(stage(&mut f).await.unwrap(), Launch::Advanced));
        assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["g.txt"]);
        assert_eq!(stop_rounds(&f.state, &f.scratch).await, 0);
        let paths = git::unmerged_paths(&f.scratch).await.unwrap();
        let mut round = f.state.rebase_fixes[0].clone();
        round.index_outside = Some(
            round_baseline(&mut f.state, &f.scratch, &paths)
                .await
                .unwrap(),
        );
        round.paths = paths;
        round.stop = rebase_head(&f.scratch).await;
        f.state.rebase_fixes.push(round);
        assert_eq!(stop_rounds(&f.state, &f.scratch).await, 1);
        std::fs::write(f.scratch.join("g.txt"), "upstream g\ng\n").unwrap();
        assert_eq!(
            rebase_with_fixer(&mut f.state, &f.scratch, "topic", "main")
                .await
                .unwrap(),
            Rebased::Applied
        );
        assert_eq!(
            git::git(&f.state.repo, &["show", "topic:f.txt"])
                .await
                .unwrap(),
            BOTH.trim_end()
        );
        assert_eq!(
            git::git(&f.state.repo, &["show", "topic:g.txt"])
                .await
                .unwrap(),
            "upstream g\ng"
        );
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn resume_after_staging_denial_uses_the_saved_target_when_upstream_moves() {
        let mut f = fixture(true).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        let saved = rebase_target(&f.scratch).await.unwrap();
        let lock = git::git(&f.scratch, &["rev-parse", "--git-path", "index.lock"])
            .await
            .unwrap();
        let lock = f.scratch.join(lock);
        std::fs::write(&lock, "").unwrap();
        assert!(
            stage(&mut f)
                .await
                .unwrap_err()
                .is::<crate::repo_lock::ResourceBlocked>()
        );
        std::fs::remove_file(lock).unwrap();
        std::fs::write(f.state.repo.join("new-upstream"), "new base\n").unwrap();
        sh(&f.state.repo, &["add", "new-upstream"]);
        sh(&f.state.repo, &["commit", "-qm", "new upstream"]);
        assert_ne!(saved, git::rev_parse(&f.state.repo, "main").await.unwrap());
        assert_eq!(
            rebase_with_fixer(&mut f.state, &f.scratch, "topic", "main")
                .await
                .unwrap(),
            Rebased::Applied
        );
        assert!(git::is_ancestor(&f.state.repo, &saved, "topic").await);
        assert!(git::is_ancestor(&f.state.repo, "main", "topic").await);
        assert_eq!(
            git::git(&f.state.repo, &["show", "topic:f.txt"])
                .await
                .unwrap(),
            BOTH.trim_end()
        );
        assert_eq!(f.state.rebase_fixes.len(), 1);
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn unrelated_edits_and_unexpected_unmerged_paths_stage_nothing() {
        let mut f = fixture(true).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        std::fs::write(f.scratch.join("stray.txt"), "x\n").unwrap();
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(&out, Launch::Nothing(why) if why.contains("stray.txt")));
        assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["f.txt"]);
        std::fs::remove_file(f.scratch.join("stray.txt")).unwrap();

        // The round only ever covered some other path.
        f.state.rebase_fixes[0].paths = vec!["other.txt".into()];
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(&out, Launch::Nothing(why) if why.contains("not part of")));
        assert!(git::rebase_in_progress(&f.scratch).await);
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn a_denied_index_write_is_an_infrastructure_stop_that_keeps_everything() {
        let mut f = fixture(true).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        let lock = git::git(&f.scratch, &["rev-parse", "--git-path", "index.lock"])
            .await
            .unwrap();
        let lock = f.scratch.join(lock);
        std::fs::write(&lock, "").unwrap();

        let err = stage(&mut f).await.unwrap_err();
        assert!(err.is::<crate::repo_lock::ResourceBlocked>(), "{err:#}");
        let block = f.state.resource_block.clone().expect("a resource block");
        assert_eq!(block.kind, "git metadata write");
        assert!(f.state.rebase_fixes[0].infra.is_some());
        assert_eq!(f.state.rebase_fixes.len(), 1, "no round was spent");
        assert!(git::rebase_in_progress(&f.scratch).await);
        assert_eq!(f.read("f.txt"), BOTH);

        // A later pass (the lock gone) finishes the same stop point.
        std::fs::remove_file(&lock).unwrap();
        f.state.resource_block = None;
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(out, Launch::Advanced));
        assert!(!git::rebase_in_progress(&f.scratch).await);
        assert_eq!(f.read("f.txt"), BOTH);
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn a_pass_interrupted_after_staging_is_continued_not_repeated() {
        let mut f = fixture(true).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        sh(&f.scratch, &["add", "f.txt"]);
        f.state.rebase_fixes[0].launcher = Some(crate::run::LauncherStep::Staged);
        assert!(git::unmerged_paths(&f.scratch).await.unwrap().is_empty());
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(out, Launch::Advanced));
        assert!(!git::rebase_in_progress(&f.scratch).await);
        assert_eq!(topic_subjects(&f).await, vec!["topic g", "topic test"]);
        // Nothing is left to do on a second pass.
        let out = stage(&mut f).await.unwrap();
        assert!(matches!(out, Launch::Nothing(_)));
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn recorded_conflict_edits_after_interrupted_staging_are_restaged() {
        for deleted in [false, true] {
            let mut f = fixture(true).await;
            std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
            sh(&f.scratch, &["add", "f.txt"]);
            f.state.rebase_fixes[0].launcher = Some(crate::run::LauncherStep::Staged);
            assert!(git::unmerged_paths(&f.scratch).await.unwrap().is_empty());

            let revised = format!("{BOTH}additional test\n");
            if deleted {
                std::fs::remove_file(f.scratch.join("f.txt")).unwrap();
            } else {
                std::fs::write(f.scratch.join("f.txt"), &revised).unwrap();
            }
            let out = stage(&mut f).await.unwrap();
            assert!(matches!(out, Launch::Advanced));
            assert!(!git::rebase_in_progress(&f.scratch).await);
            assert_eq!(topic_subjects(&f).await, vec!["topic g", "topic test"]);
            if deleted {
                assert!(
                    git::git(&f.scratch, &["ls-tree", "HEAD", "--", "f.txt"])
                        .await
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert_eq!(f.read("f.txt"), revised);
                assert_eq!(
                    git::git(&f.scratch, &["show", "HEAD:f.txt"]).await.unwrap(),
                    revised.trim_end()
                );
            }
            assert!(status_entries(&f.scratch).await.unwrap().is_empty());
            assert_eq!(f.state.rebase_fixes.len(), 1, "no further round was spent");
            std::fs::remove_dir_all(f.state.dir()).ok();
        }
    }

    #[tokio::test]
    async fn a_cancelled_run_stages_nothing_and_keeps_the_resolved_files() {
        let mut f = fixture(true).await;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        let cancelled = crate::repo_lock::WaitPolicy::new(Duration::from_secs(5), || true);
        let err = cancelled.scope(stage(&mut f)).await.unwrap_err();
        assert!(err.is::<crate::repo_lock::ResourceBlocked>());
        assert!(git::rebase_in_progress(&f.scratch).await);
        assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["f.txt"]);
        assert_eq!(f.read("f.txt"), BOTH);
        std::fs::remove_dir_all(f.state.dir()).ok();
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_launcher_git_terminates_descendants_before_releasing_coordination() {
        for abort in [true, false] {
            cancelled_git_operation(abort).await;
        }
    }

    #[cfg(unix)]
    async fn cancelled_git_operation(abort: bool) {
        let mut f = fixture(true).await;
        let scratch = f.scratch.clone();
        let state = &mut f.state;
        let policy = crate::config::test_coordination().unwrap();
        let common = crate::repo_lock::common_dir(&scratch)
            .await
            .unwrap()
            .unwrap();
        std::fs::write(common.join(&policy.contract_file), &policy.contract_token).unwrap();
        state.config.coordination = Some(policy.clone());
        let wait = crate::repo_lock::WaitPolicy::default();
        let lease = crate::repo_lock::SeatLease::operation_with(&scratch, Some(&policy), &wait)
            .await
            .unwrap();
        sh(
            &scratch,
            &[
                "config",
                "alias.block",
                "!echo $$ > operation.pid; exec sleep 30",
            ],
        );
        let cwd = scratch.clone();
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal = cancelled.clone();
        let cancellation = crate::repo_lock::WaitPolicy::new(Duration::from_secs(5), move || {
            signal.load(std::sync::atomic::Ordering::SeqCst)
        });
        let operation =
            tokio::spawn(async move { cancellation.scope(mutate(&cwd, &["block"], lease)).await });
        let pid_file = scratch.join("operation.pid");
        let pid: i32 = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = text.trim().parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(crate::retention::OutputLease::take(&scratch, true).is_err());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                crate::repo_lock::SeatLease::operation_with(&scratch, Some(&policy), &wait)
            )
            .await
            .is_err()
        );
        if abort {
            operation.abort();
            assert!(operation.await.unwrap_err().is_cancelled());
        } else {
            cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
            let error = tokio::time::timeout(Duration::from_secs(5), operation)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.is::<crate::repo_lock::ResourceBlocked>());
        }
        // Acquiring the same lease waits for the independent cleanup owner.
        let _next = crate::repo_lock::SeatLease::operation_with(&scratch, Some(&policy), &wait)
            .await
            .unwrap();
        // SAFETY: signal zero only checks whether the known test process lives.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let _output = crate::retention::OutputLease::take(&scratch, true).unwrap();
        assert!(git::rebase_in_progress(&scratch).await);
        assert_eq!(state.rebase_fixes.len(), 1);
        std::fs::remove_dir_all(state.dir()).ok();
    }

    #[tokio::test]
    async fn recovery_keeps_sixteen_commits_and_automatic_changes() {
        let mut f = fixture_with(true, "f.txt", 16).await;
        f.state.config.graph.review_rounds = 1;
        std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
        let original = git::rev_parse(&f.state.repo, "topic").await.unwrap();
        let subjects = git::git(&f.scratch, &["log", "--format=%s", "main..topic"])
            .await
            .unwrap();
        assert_eq!(
            rebase_with_fixer(&mut f.state, &f.scratch, "topic", "main")
                .await
                .unwrap(),
            Rebased::Applied
        );
        let heads = git::git(&f.state.repo, &["rev-list", "--count", "main..topic"])
            .await
            .unwrap();
        assert_eq!(heads, "16");
        assert_eq!(
            git::git(&f.state.repo, &["show", "topic:f.txt"])
                .await
                .unwrap(),
            BOTH.trim_end()
        );
        assert_eq!(
            git::git(&f.state.repo, &["show", "topic:automatic"])
                .await
                .unwrap(),
            "topic applied cleanly"
        );
        assert_eq!(
            git::git(&f.state.repo, &["log", "--format=%s", "main..topic"])
                .await
                .unwrap(),
            subjects
        );
        for n in 2..16 {
            assert_eq!(
                git::git(&f.state.repo, &["show", &format!("topic:commit-{n}")])
                    .await
                    .unwrap(),
                n.to_string()
            );
        }
        assert_eq!(f.state.rebase_fixes.len(), 1);
        assert_ne!(
            git::rev_parse(&f.state.repo, "topic").await.unwrap(),
            original
        );
        std::fs::remove_dir_all(f.state.dir()).ok();
    }

    #[tokio::test]
    async fn recovery_handles_literal_conflict_paths_with_whitespace() {
        // Win32 rejects trailing spaces and control characters in file names.
        // Exercise literal pathspecs there with valid whitespace and brackets;
        // retain trailing whitespace and control-character coverage on Unix.
        let paths = [" spaced file name", "日本語 [literal].txt", "日本語.txt"].into_iter();
        #[cfg(not(windows))]
        let paths = paths.chain([" spaced file ", "tab\tand\nnewline"]);
        for path in paths {
            let mut f = fixture_with(true, path, 2).await;
            assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), [path]);
            std::fs::write(f.scratch.join(path), BOTH).unwrap();
            assert!(matches!(stage(&mut f).await.unwrap(), Launch::Advanced));
            assert_eq!(f.read(path), BOTH);
            assert!(git::unmerged_paths(&f.scratch).await.unwrap().is_empty());
            std::fs::remove_dir_all(f.state.dir()).ok();
        }
    }

    #[tokio::test]
    async fn changed_rebase_identity_blocks_staging_and_keeps_resolution() {
        for name in ["orig-head", "onto"] {
            let mut f = fixture(true).await;
            std::fs::write(f.scratch.join("f.txt"), BOTH).unwrap();
            let metadata = git::git(
                &f.scratch,
                &["rev-parse", "--git-path", &format!("rebase-merge/{name}")],
            )
            .await
            .unwrap();
            std::fs::write(
                f.scratch.join(metadata),
                "0000000000000000000000000000000000000000\n",
            )
            .unwrap();
            let error = stage(&mut f).await.unwrap_err();
            assert!(error.is::<crate::repo_lock::ResourceBlocked>());
            assert!(error.to_string().contains(name));
            assert_eq!(git::unmerged_paths(&f.scratch).await.unwrap(), ["f.txt"]);
            assert_eq!(f.read("f.txt"), BOTH);
            assert_eq!(f.state.rebase_fixes.len(), 1);
            std::fs::remove_dir_all(f.state.dir()).ok();
        }
    }
}
