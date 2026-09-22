//! Terminal rendering.
//!
//! A run produces a lot of state; the report exists so the operator can decide
//! what to do next without opening `run.json`. It leads with the disagreement,
//! because that is the part that carries information: three judges agreeing
//! tells you nothing the winner's diff does not.
//!
//! Colour is a six-line local implementation rather than a crate. The
//! alternatives all decide *for* you whether the stream supports colour, which
//! makes the output untestable — `assert!(text.contains("winner  A"))` fails on
//! an escape sequence the test never asked for.
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::{MergeMode, MergeStyle};
use crate::run::{
    CommandOutcome, ContinuationOutcome, E2eStatus, GateStatus, JobStatus, OperatorFixOutcome,
    RunState, RunStatus, tail,
};
use crate::stats::Stats;
use crate::verdict::ReviewVote;

static COLOR: AtomicBool = AtomicBool::new(true);

/// Turn colour on or off for every subsequent render.
pub fn set_color(on: bool) {
    COLOR.store(on, Ordering::Relaxed);
}

fn paint(text: &str, code: &str) -> String {
    if COLOR.load(Ordering::Relaxed) {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn bold(t: &str) -> String {
    paint(t, "1")
}
fn dim(t: &str) -> String {
    paint(t, "2")
}
fn red(t: &str) -> String {
    paint(t, "31")
}
fn green(t: &str) -> String {
    paint(t, "32")
}
fn yellow(t: &str) -> String {
    paint(t, "33")
}
fn cyan(t: &str) -> String {
    paint(t, "36")
}

/// Colour for a status word.
///
/// `Stalled` is deliberately not green: a run whose judges were taken out by a
/// rate limit must not look like a healthy `Ready` in a one-line listing.
///
/// A `Ready` reached via `[merge] mode = "none"` is a second case that must
/// not look like a plain `Ready`: that run is done for good, never picked up
/// by the PR-polling merge watcher or anything else, while an ordinary
/// `Ready` (a PR closed without merging, an already-concluded re-entry) may
/// still be a live landing candidate. See [`RunState::unmerged_by_design`].
fn status_word(state: &RunState) -> String {
    if state.unmerged_by_design() {
        return cyan("unmerged (no-op by design)");
    }
    let text = state.status.display_label();
    match state.status {
        RunStatus::Merged => bold(&green(text)),
        RunStatus::Ready => green(text),
        RunStatus::Stalled => bold(&yellow(text)),
        RunStatus::Blocked => yellow(text),
        RunStatus::Failed => red(text),
        // Not `Failed`'s red: every candidate agreed, with evidence, that
        // nothing belongs in this worktree — the opposite of a run that
        // could not do the work. See `RunStatus::VerifiedNoop`'s own doc.
        RunStatus::VerifiedNoop => cyan(text),
        _ => cyan(text),
    }
}

/// Colour for a reviewer vote — the same scale a finding's severity gets:
/// green for no reservations, yellow for proceed-but-look-at-this, red for a
/// vote that says stop.
fn vote_tag(vote: ReviewVote) -> String {
    let text = vote.label();
    match vote {
        ReviewVote::Approve => green(text),
        ReviewVote::ApproveWithFindings => yellow(text),
        ReviewVote::Reject => red(text),
    }
}

/// One-line summary, for `magi list`.
pub fn line(state: &RunState) -> String {
    let winner = state
        .tally
        .as_ref()
        .map_or("-".to_owned(), |t| t.winner.to_string());
    let agent = state.winner().map_or("-", |c| c.agent.as_str());
    // A below-quorum verdict carries an explicit stamp so a row in a listing
    // reads "stalled" and "2/3 judges" without opening the report.
    let quorum = match state.tally.as_ref() {
        Some(t) if !t.met_quorum => format!(
            "  {}",
            bold(&red(&format!("quorum {}/{}", t.present, t.judges)))
        ),
        Some(t) if t.present > 0 && t.present < t.judges => format!(
            "  {}",
            yellow(&format!("judges {}/{}", t.present, t.judges))
        ),
        _ => String::new(),
    };
    format!(
        "{}  {:<20}  {:>2}c {:>2}j  win {} ({}){quorum}  {}",
        dim(&state.id),
        status_word(state),
        state.candidates.len(),
        state.judgements.len(),
        winner,
        agent,
        first_line(&state.instruction)
    )
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > 68 {
        format!("{}…", line.chars().take(67).collect::<String>())
    } else {
        line.to_owned()
    }
}

fn short(commit: &str) -> String {
    commit.chars().take(7).collect()
}

/// A short badge for a [`crate::run::ContinuationRecord`], distinguishing
/// "the seat's own report needed to be resumed" from "there was nothing to
/// address" — a fixer that has never needed this stays silent here, exactly
/// as a record predating the feature (`continuation: None`) does too.
fn continuation_note(c: &crate::run::ContinuationRecord) -> String {
    match c.outcome {
        ContinuationOutcome::NotNeeded => String::new(),
        ContinuationOutcome::Resumed => format!(" [resumed x{}]", c.attempts),
        ContinuationOutcome::Exhausted => format!(" [continuation exhausted x{}]", c.attempts),
        ContinuationOutcome::QuotaLost => " [continuation: quota]".to_owned(),
        ContinuationOutcome::NoSession => " [no session to resume]".to_owned(),
    }
}

/// Commands seats' own CLIs reported running, across every node — see
/// [`crate::run::JobRecord`]. Read-only: renders whatever `run.json` already
/// holds, does not query anything live and does not spawn an agent.
///
/// A CLI this crate has no adapter for (every backend but Codex, as of this
/// writing) never appears here at all — silence is "no evidence", not "no
/// jobs ran", which the trailing coverage line exists to say once rather
/// than per seat.
///
/// There is deliberately no "running" state anywhere in this. A command a
/// CLI never reported finishing has no way to be told apart from one that
/// never started at all: the CLI that would report it has, by construction,
/// already stopped talking (killed by a timeout, or a park) by the time that
/// question matters, so no event — real or guessed at — could ever answer
/// it. Guessing at an unconfirmed event shape to manufacture a "running"
/// entry is exactly the fabrication this feature must not do; recording that
/// limit instead is what the task asks for here. `magi show`'s own
/// seat-completion state ([`active_seats`]) still answers a related but
/// different question honestly — "has this seat's own turn answered yet" —
/// and stays the right place to look for that.
fn jobs_section(state: &RunState) -> String {
    let mut s = String::new();
    if state.jobs.is_empty() {
        // Not silent when it would matter: a run whose roster can actually
        // report this (Codex, today) but has not yet says so explicitly, so
        // "no adapter for this backend" and "nothing reported yet" are never
        // the same blank space to a reader.
        if state
            .config
            .agents
            .iter()
            .any(|a| a.kind == crate::config::AgentKind::Codex)
        {
            let _ = writeln!(
                s,
                "\n{}",
                dim(
                    "background jobs: no completed command evidence yet for this run (see \
                     active seats above for what is still mid-turn)"
                )
            );
        }
        return s;
    }
    let _ = writeln!(
        s,
        "\n{}",
        bold("background jobs (from each seat's own CLI)")
    );
    let mut by_seat: std::collections::BTreeMap<(&str, &str), Vec<&crate::run::JobRecord>> =
        std::collections::BTreeMap::new();
    for j in &state.jobs {
        by_seat
            .entry((j.node.as_str(), j.seat.as_str()))
            .or_default()
            .push(j);
    }
    for ((node, seat), records) in by_seat {
        let _ = writeln!(s, "  {node}/{seat}");
        for j in records {
            let status = match j.status {
                JobStatus::Completed => green("completed"),
                JobStatus::Failed => red("failed"),
                JobStatus::Unknown => yellow("unknown"),
            };
            let _ = writeln!(
                s,
                "    {} {}{}{}  checked {}",
                dim(&j.id),
                status,
                j.exit_code
                    .map_or(String::new(), |c| format!(" (exit {c})")),
                j.round.map_or(String::new(), |r| format!("  round {r}")),
                j.checked_at
                    .to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M:%S")
            );
            let desc = first_line(&j.description);
            if !desc.trim().is_empty() {
                let _ = writeln!(s, "      $ {desc}");
            }
            let summary = first_line(&j.result_summary);
            if !summary.trim().is_empty() {
                let _ = writeln!(s, "      {}", dim(&summary));
            }
        }
    }
    let _ = writeln!(
        s,
        "  {}",
        dim(
            "(adapter coverage: codex only today; other backends, and a command a CLI never \
             reported finishing, leave no entry here — that is unknown, never \"nothing ran\")"
        )
    );
    s
}

/// Full report for one run.
pub fn run(state: &RunState) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{} {}  {}",
        bold("magi run"),
        bold(&state.id),
        status_word(state)
    );
    let _ = writeln!(
        s,
        "  repo    {} ({} @ {})",
        state.repo.display(),
        state.base_branch,
        short(&state.base_commit)
    );
    let _ = writeln!(s, "  created {}", state.created_local());
    let _ = writeln!(s, "  task    {}", first_line(&state.instruction));
    let _ = writeln!(s, "  state   {}", state.dir().display());

    let _ = writeln!(s, "\n{}", bold("candidates"));
    for c in &state.candidates {
        let flag = match (&c.failed, c.empty, &c.verified_noop) {
            (Some(e), _, _) => red(&format!("failed: {e}")),
            // Neither red (nothing failed) nor plain yellow "no change" (that
            // reads as an unexplained loss): the candidate gave a reason a
            // human still has to check, not a claim magi itself confirmed.
            (None, true, Some(_)) => cyan("agent-verified no-op (unconfirmed)"),
            (None, true, None) => yellow("no change"),
            _ => format!("{} files, {} commits", c.files, c.commits),
        };
        let crown = if state.tally.as_ref().is_some_and(|t| t.winner == c.label) {
            bold(&green("  <- winner"))
        } else {
            String::new()
        };
        let _ = writeln!(
            s,
            "  {}  {:<12} {:<30} {:>5}s{}",
            bold(&c.label.to_string()),
            c.agent,
            flag,
            c.duration_ms / 1000,
            crown
        );
        if let Some(evidence) = &c.verified_noop {
            let _ = writeln!(s, "      {}", dim(&first_line(evidence)));
        } else if !c.summary.trim().is_empty() {
            // The candidate's own account of what it did and why — the
            // "## SUMMARY" `prompt::implement` asks for — was recorded on
            // every run but never surfaced here, which left `magi show`
            // silent about it even when the summary was the whole point (an
            // implementer explaining *why* it wrote nothing, short of a
            // verified no-op's own line above). One line, matching the
            // house style other prose fields get in this report (see the
            // review findings' `detail` below); the rest is in `run.json`.
            let _ = writeln!(s, "      {}", dim(&first_line(&c.summary)));
        }
    }

    if !state.judgements.is_empty() {
        let _ = writeln!(s, "\n{}", bold("blind judging"));
        for j in &state.judgements {
            match &j.failed {
                Some(e) => {
                    let _ = writeln!(
                        s,
                        "  judge {}  {}",
                        j.judge,
                        red(&format!("no ranking: {e}"))
                    );
                }
                None => {
                    let _ = writeln!(
                        s,
                        "  judge {}  {:<12} {}  confidence {}",
                        j.judge,
                        j.agent,
                        bold(&j.ranking.iter().collect::<String>()),
                        j.confidence.map_or("-".to_owned(), |c| c.to_string())
                    );
                }
            }
        }
    }

    if let Some(t) = &state.tally {
        if t.deliberated {
            let _ = writeln!(s, "\n{}", bold("deliberation"));
            for round in &state.deliberation {
                for turn in &round.turns {
                    let _ = writeln!(
                        s,
                        "  r{} judge {} -> {}",
                        round.round,
                        turn.judge,
                        turn.tentative.map_or("-".to_owned(), |c| c.to_string())
                    );
                }
            }
        }

        if !state.votes.is_empty() {
            let _ = writeln!(s, "\n{}", bold("final votes (collected privately)"));
            for v in &state.votes {
                let _ = writeln!(
                    s,
                    "  judge {}  {:<12} {}{}",
                    v.judge,
                    v.agent,
                    bold(&v.vote.unwrap_or('?').to_string()),
                    if v.changed {
                        yellow("  (changed after deliberation)")
                    } else {
                        String::new()
                    }
                );
            }
        }

        let _ = writeln!(s, "\n{}", bold("tally"));
        // A tally with no panel (`uncontested`) must not fall through the
        // judges/first-choice/after-votes lines below: they are written
        // unconditionally and every one of them reads, in the words a panel
        // that collapsed would also produce, as a run that lost its judges
        // rather than one that never needed them.
        match &t.uncontested {
            Some(reason) => {
                let _ = writeln!(
                    s,
                    "  judging       {}",
                    cyan(&format!("not needed — {reason}"))
                );
            }
            None => {
                let _ = writeln!(
                    s,
                    "  judges        {} present{}",
                    if t.met_quorum {
                        green(&format!("{}/{}", t.present, t.judges))
                    } else {
                        red(&format!("{}/{}", t.present, t.judges))
                    },
                    if t.quorum > 0 {
                        format!(" ({quorum} required)", quorum = t.quorum)
                    } else {
                        String::new()
                    }
                );
                if !t.met_quorum {
                    let _ = writeln!(
                        s,
                        "  {}",
                        bold(&red("BELOW QUORUM — verdict is not trustworthy"))
                    );
                }
                let _ = writeln!(
                    s,
                    "  first choice  {}",
                    t.first_choice
                        .iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect::<Vec<_>>()
                        .join("  ")
                );
                let _ = writeln!(
                    s,
                    "  initial       {}",
                    match (t.rankings, t.unanimous_initial) {
                        (0, _) => red("no usable ranking"),
                        (1, _) => yellow("one usable ranking - not a consensus"),
                        (_, true) => green("unanimous"),
                        (_, false) => yellow("split"),
                    }
                );
                let _ = writeln!(
                    s,
                    "  after votes   {}  ({} judge(s) moved)",
                    if t.unanimous_final {
                        green("unanimous")
                    } else {
                        yellow("still split")
                    },
                    t.changed_votes
                );
                if let Some(tb) = &t.tie_break {
                    let _ = writeln!(s, "  tie break     {tb}");
                }
            }
        }
        if !state.quota.is_empty() {
            let _ = writeln!(
                s,
                "  rate limited  {}",
                state
                    .quota
                    .iter()
                    .map(|q| q.seat.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let _ = writeln!(s, "  winner        {}", bold(&green(&t.winner.to_string())));
    }

    if !state.reviews.is_empty() {
        let _ = writeln!(s, "\n{}", bold("review + verification"));
        for r in &state.reviews {
            let raised: usize = r.reviews.iter().map(|x| x.findings.len()).sum();
            // A build/link failure is not a verdict on the patch (a shared
            // `CARGO_TARGET_DIR` link race looks exactly like one), so it
            // must not read the same as a real test failure. And a deferred
            // round is not a passed one: `e2e.is_empty()` alone cannot tell
            // "not configured" from "skipped on purpose" apart, which is
            // exactly why `e2e_status` exists rather than reading `e2e`
            // directly here.
            let e2e = match r.e2e_status() {
                E2eStatus::NotConfigured => dim("no e2e"),
                E2eStatus::Deferred => yellow(&format!(
                    "e2e deferred{}",
                    r.e2e_defer_reason
                        .as_deref()
                        .map(|why| format!(" ({why})"))
                        .unwrap_or_default()
                )),
                E2eStatus::Passed => green("e2e green"),
                E2eStatus::Failed if r.e2e.iter().any(CommandOutcome::build_failed) => {
                    yellow("e2e could not run (build/link failure)")
                }
                E2eStatus::Failed => red("e2e RED"),
                // Magi's own admission it could not get a command to run —
                // never the same yellow/red a real attempt earns, since
                // nothing here is evidence about the patch (see
                // `E2eStatus::ResourceBlocked`'s own doc).
                E2eStatus::ResourceBlocked => {
                    yellow("e2e could not run (shared build cache unavailable)")
                }
            };
            let e2e = if r.verify_retried {
                format!("{e2e}, retried once")
            } else {
                e2e
            };
            // Three distinct facts, not two: a round can be *open* (blocking
            // findings still standing), *incomplete* (a seat never answered,
            // so what the round says is missing input) or genuinely clean.
            let status = if r.incomplete() {
                yellow("incomplete")
            } else if r.clean {
                green("clean")
            } else {
                yellow("open")
            };
            // A missing seat must stay visible even when `warn` policy let
            // the round gate as clean: the reader should never have to take
            // "clean" on faith when the panel wasn't full.
            let panel = if r.incomplete() {
                let missing: Vec<String> = r
                    .reviews
                    .iter()
                    .filter_map(|x| {
                        x.failed
                            .as_ref()
                            .map(|why| format!("review-{}: {why}", x.reviewer))
                    })
                    .collect();
                format!(
                    "  {}/{} reviewers answered ({})",
                    r.answered,
                    r.expected,
                    missing.join(", ")
                )
            } else {
                String::new()
            };
            // The verdict is the one thing this loop cannot derive from
            // `blocking`/`e2e` alone: three seats can agree there is nothing
            // blocking and still split on whether the patch is fine to
            // proceed as-is, which is exactly the disagreement a vote exists
            // to surface.
            let verdict = r.verdict.map_or(String::new(), |v| {
                format!(
                    ", verdict {}{}",
                    vote_tag(v),
                    if r.vote_split { " (panel split)" } else { "" }
                )
            });
            let _ = writeln!(
                s,
                "  round {}  {} @ {}{}{panel}  {raised} finding(s), {} blocking, {e2e}{verdict}{}",
                r.round,
                status,
                short(&r.head),
                r.verified_head.as_ref().map_or(String::new(), |head| {
                    format!(
                        " (verified @ {}{})",
                        short(head),
                        r.verified_at.map_or(String::new(), |t| format!(
                            " on {}",
                            t.to_zoned(jiff::tz::TimeZone::system())
                                .strftime("%Y-%m-%d %H:%M:%S")
                        ))
                    )
                }),
                r.blocking,
                r.fix.as_ref().map_or(String::new(), |f| {
                    let tree = if r.progressed {
                        green("changed")
                    } else {
                        yellow("unchanged")
                    };
                    let cont = f
                        .continuation
                        .as_ref()
                        .map_or(String::new(), continuation_note);
                    match &f.failed {
                        // Never the same shape as "N addressed / M rejected": the
                        // fixer's diff may well have landed (see the `fix` node's
                        // own event), but whether it addressed anything is
                        // unknown, not zero.
                        Some(reason) => format!(
                            "  fix: {}, tree {tree}{}{cont}",
                            yellow(&format!("adoption report lost ({reason})")),
                            if f.committed {
                                String::new()
                            } else {
                                red(" (NO COMMIT)")
                            }
                        ),
                        None => format!(
                            "  fix: {} addressed / {} rejected, tree {tree}{}{cont}",
                            f.addressed.len(),
                            f.rejected.len(),
                            if f.committed {
                                String::new()
                            } else {
                                red(" (NO COMMIT)")
                            }
                        ),
                    }
                })
            );
            // The aggregate label above (`e2e`) says the round's overall
            // verdict; it never named the commands themselves, so a round
            // with more than one `verify.e2e` command left no way to tell
            // which one actually failed or was blocked without opening
            // `run.json` by hand — the same gap `gate`'s own per-command
            // listing above already closes for the final gate.
            for o in &r.e2e {
                let label = if o.resource_blocked {
                    yellow("blocked")
                } else if o.ok() {
                    green("pass")
                } else {
                    red("FAIL")
                };
                let _ = writeln!(s, "    {label}  {}", o.command);
                if !o.ok() {
                    let _ = writeln!(s, "{}", dim(&tail(&o.output_tail, 2_000)));
                }
            }
            for rec in &r.reviews {
                if let Some(vote) = rec.vote {
                    let _ = writeln!(s, "      review-{} vote {}", rec.reviewer, vote_tag(vote));
                }
                for f in &rec.findings {
                    let adopted = r
                        .fix
                        .as_ref()
                        .is_some_and(|fix| fix.addressed.contains(&f.id));
                    let _ = writeln!(
                        s,
                        "      {} [{:?}] {}{}",
                        dim(&f.id),
                        f.severity,
                        f.title,
                        if adopted {
                            green("  fixed")
                        } else {
                            String::new()
                        }
                    );
                }
            }
            if let Some(fix) = &r.fix {
                for rej in &fix.rejected {
                    let _ = writeln!(
                        s,
                        "      {} {}: {}",
                        dim(&rej.id),
                        yellow("declined"),
                        rej.why
                    );
                }
            }
            // Reconsideration only ever has entries when the round's initial
            // votes split — an empty list here means the panel agreed the
            // first time, same as an empty `deliberation` for judges.
            if !r.reconsideration.is_empty() {
                let _ = writeln!(s, "      {}", dim("reconsideration:"));
                for rv in &r.reconsideration {
                    match rv.vote {
                        Some(v) => {
                            let _ = writeln!(
                                s,
                                "        review-{} -> {}  {}",
                                rv.reviewer,
                                vote_tag(v),
                                rv.reason
                            );
                        }
                        None => {
                            let _ = writeln!(
                                s,
                                "        review-{} -> {}",
                                rv.reviewer,
                                red(&format!(
                                    "no revote ({})",
                                    rv.failed.as_deref().unwrap_or("unknown")
                                ))
                            );
                        }
                    }
                }
            }
        }
        if state.handed_off_with_open_findings() {
            let _ = writeln!(
                s,
                "\n  {}",
                yellow(&format!(
                    "handed off with {} finding(s) still open — gate and e2e were green; \
                     see above for what a person should still look at",
                    state.open_findings().len()
                ))
            );
        }
    }

    if !state.operator_fixes.is_empty() {
        let _ = writeln!(s, "\n{}", bold("operator fix(es)"));
        for (i, req) in state.operator_fixes.iter().enumerate() {
            let _ = writeln!(
                s,
                "  [{}] {} finding(s) at {}{}",
                i + 1,
                req.findings.len(),
                req.requested_at
                    .to_zoned(jiff::tz::TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M:%S"),
                if req.stale {
                    yellow("  stale head, --allow-stale used")
                } else {
                    String::new()
                }
            );
            let _ = writeln!(s, "      reason: {}", req.reason);
            for f in &req.findings {
                let outcome = match &f.outcome {
                    OperatorFixOutcome::Pending => yellow("pending"),
                    OperatorFixOutcome::Addressed => green("addressed"),
                    OperatorFixOutcome::Rejected { why } => red(&format!("rejected: {why}")),
                    OperatorFixOutcome::Unreported => {
                        red("unreported — no adoption report came back")
                    }
                };
                let _ = writeln!(
                    s,
                    "      {} [{:?}] {}  {outcome}",
                    dim(&f.id),
                    f.severity,
                    f.title
                );
            }
            match &req.follow_up_review_run {
                Some(id) => {
                    let _ = writeln!(s, "      re-verified by run {id}");
                }
                None if req.fix.as_ref().is_some_and(|fx| fx.committed) => {
                    let _ = writeln!(
                        s,
                        "      {}",
                        red("committed, but the follow-up review could not be opened")
                    );
                }
                None => {
                    let _ = writeln!(s, "      no change committed; nothing to re-verify");
                }
            }
        }
    }

    if let Some(bs) = &state.base_sync {
        let _ = writeln!(s, "\n{}", bold("base sync"));
        let status = if let Some(c) = &bs.conflict {
            red(&format!("conflict: {}", first_line(c)))
        } else if bs.behind == 0 {
            green("in sync")
        } else {
            yellow(&format!("{} commit(s) behind, not yet rebased", bs.behind))
        };
        let _ = writeln!(
            s,
            "  {} @ {}  {status}{}",
            state.base_branch,
            short(&bs.tip),
            if bs.attempts > 0 {
                format!("  ({} rebase attempt(s))", bs.attempts)
            } else {
                String::new()
            }
        );
    }

    // `state.gate.is_empty()` alone cannot tell "never ran" apart from "ran
    // with nothing configured" — see `RunState::gate_status`'s own doc — so
    // this reads the accessor rather than the raw list.
    match state.gate_status() {
        GateStatus::NotRun => {}
        GateStatus::PassedWithNoCommands => {
            let _ = writeln!(s, "\n{}", bold("gate"));
            let _ = writeln!(s, "  {}  no gate commands configured", green("pass"));
        }
        GateStatus::Passed | GateStatus::Failed => {
            let _ = writeln!(s, "\n{}", bold("gate"));
            for o in &state.gate {
                let _ = writeln!(
                    s,
                    "  {}  {}",
                    if o.ok() { green("pass") } else { red("FAIL") },
                    o.command
                );
                if !o.ok() {
                    let _ = writeln!(s, "{}", dim(&tail(&o.output_tail, 2_000)));
                }
            }
        }
    }

    if let Some(m) = &state.merge {
        let _ = writeln!(s, "\n{}", bold("merge"));
        if m.mode == MergeMode::None {
            // `ok: true` here means "magi did nothing, as configured", not
            // "landed" — a green `ok` next to a shell command reads as done,
            // and the branch is still sitting unmerged.
            let _ = writeln!(
                s,
                "  mode None  {}",
                cyan("not landed — nothing to do by design")
            );
            if let Some(w) = state.winner() {
                let _ = writeln!(
                    s,
                    "  branch {} still exists, unmerged into {}",
                    w.branch, state.base_branch
                );
            }
            // The squash caveat only applies to that one style: `--no-ff` and
            // `--ff-only` never inherit a candidate's placeholder subject,
            // since neither ever discards the pull request body `message`
            // that `manual_merge_command` (graph.rs) already puts on the
            // squash commit's `-m`.
            let _ = writeln!(
                s,
                "  rebase onto {} before merging by hand{}",
                state.base_branch,
                if state.config.merge.style == MergeStyle::Squash {
                    ", and pass an explicit commit message — a squash merge \
                     otherwise inherits the candidate's placeholder subject"
                } else {
                    ""
                }
            );
            let _ = writeln!(s, "  {}", m.detail.lines().next().unwrap_or(""));
        } else {
            let _ = writeln!(
                s,
                "  mode {:?}  {}\n  {}",
                m.mode,
                if m.ok {
                    green("ok")
                } else {
                    yellow("not merged")
                },
                m.detail.lines().next().unwrap_or("")
            );
        }
    }

    if !state.leaks.is_empty() {
        let _ = writeln!(s, "\n{}", bold(&yellow("blindness warnings")));
        for l in &state.leaks {
            let _ = writeln!(s, "  {} x{} in {}", l.token, l.count, l.site);
        }
    }

    if let Some(w) = state.winner()
        && !w.folded
    {
        let _ = writeln!(
            s,
            "\n{} {}\n  branch {}",
            bold("winner worktree"),
            w.worktree.display(),
            w.branch
        );
    }
    s.push_str(&jobs_section(state));
    s
}

/// The seats currently mid-answer, for `magi show` and the raw report route.
///
/// Separate from [`run`] on purpose: [`run`] is printed straight after `magi
/// run` / `magi review`'s own `execute()`, and by then this process has
/// nothing left in flight to report; the TUI does not track daemon liveness
/// either. Only a caller reading someone *else's* run — `magi show <id>`, or
/// the web UI's raw-report route — needs this, and both already know how to
/// ask whether a daemon is currently driving it.
///
/// `live` is whether a daemon's heartbeat currently names this run
/// (`daemon::is_working_on`). An [`ActiveSeat`](crate::run::ActiveSeat) left
/// behind by a killed process is not lied about as running just because
/// nobody has cleared it from disk yet — see that type's own docs for why an
/// entry alone is not proof of anything.
pub fn active_seats(state: &RunState, live: bool) -> String {
    if state.active.is_empty() {
        return String::new();
    }
    let mut s = String::new();
    let _ = writeln!(s, "\n{}", bold("running now"));
    if !live {
        let _ = writeln!(
            s,
            "  {}",
            yellow(
                "no live daemon claims this run right now — likely left behind by a killed process"
            )
        );
    }
    let now = jiff::Timestamp::now();
    for (seat, a) in &state.active {
        let retry = if a.attempt > 0 {
            format!(" retry {}", a.attempt)
        } else {
            String::new()
        };
        let _ = writeln!(
            s,
            "  {:<12} {:<12}{retry}  {}s elapsed, {}s left of {}s",
            seat,
            a.node,
            a.elapsed_secs(now),
            a.remaining_secs(now),
            a.timeout_secs
        );
    }
    s
}

/// Aggregate tables, for `magi stats`.
pub fn stats(stats: &Stats) -> String {
    let t = &stats.totals;
    let mut s = String::new();
    let _ = writeln!(s, "{}", bold("runs"));
    let _ = writeln!(
        s,
        "  {} total - {} merged, {} ready, {} blocked, {} failed ({:.0}% completion)",
        t.runs,
        t.merged,
        t.ready,
        t.blocked,
        t.failed,
        t.completion_rate()
    );
    if t.tallied > 0 {
        let _ = writeln!(
            s,
            "  {} tallied - {} split on first choice ({:.0}%), {} deliberated, \
             {} of those changed a mind, {} converged to unanimous",
            t.tallied,
            t.split,
            t.split_rate(),
            t.deliberated,
            t.minds_changed,
            t.converged
        );
    }

    if !stats.agents.is_empty() {
        let _ = writeln!(
            s,
            "\n{}",
            bold("implementation (relative, on this workload)")
        );
        let _ = writeln!(
            s,
            "  {:<14}{:>6}{:>8}{:>8}{:>8}",
            "agent", "won", "entered", "rate", "empty"
        );
        for a in &stats.agents {
            let _ = writeln!(
                s,
                "  {:<14}{:>6}{:>8}{:>7.0}%{:>8}",
                a.agent,
                a.wins,
                a.entered,
                a.win_rate(),
                a.empty
            );
        }
    }

    if !stats.reviewers.is_empty() {
        let _ = writeln!(s, "\n{}", bold("review"));
        let _ = writeln!(
            s,
            "  {:<14}{:>8}{:>10}{:>11}{:>9}{:>9}{:>9}",
            "reviewer", "rounds", "submitted", "adopted/rd", "precision", "unique", "timeout"
        );
        for r in &stats.reviewers {
            let _ = writeln!(
                s,
                "  {:<14}{:>8}{:>10}{:>11.2}{:>8.0}%{:>8.0}%{:>8.0}%",
                r.agent,
                r.rounds,
                r.submitted,
                r.adopted_per_round(),
                r.precision(),
                r.unique_rate(),
                r.timeout_rate()
            );
        }
    }

    if stats.e2e.rounds > 0 || stats.e2e.deferred > 0 {
        let _ = writeln!(s, "\n{}", bold("verification"));
        let _ = writeln!(
            s,
            "  {} rounds ran e2e, {} failed, {} of those with a clean static \
             review ({:.0}% sole detections), {} round(s) deferred it to the fixer",
            stats.e2e.rounds,
            stats.e2e.failures,
            stats.e2e.sole_detections,
            stats.e2e.sole_rate(),
            stats.e2e.deferred
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::{
        Candidate, CommandOutcome, FixRecord, MergeOutcome, ReviewRecord, ReviewRound, RunState,
        Tally,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    /// `COLOR` is process-global, so these tests cannot run concurrently.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn plain() -> MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_color(false);
        guard
    }

    fn state() -> RunState {
        // `run()` prints `state.dir()`, which reads the process-global home;
        // pinning it here keeps this test off the operator's real one. The
        // directory itself is never read, only its path printed, so nothing
        // needs to create or clean it up.
        crate::run::set_home(std::env::temp_dir().join("magi-report-test-home"));
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abcdef1234".to_owned(),
            "add retries to the uploader".to_owned(),
            Config::default(),
        );
        s.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "opus".to_owned(),
            branch: "magi/x/A".to_owned(),
            worktree: PathBuf::from("/wt/A"),
            summary: String::new(),
            stat: String::new(),
            files: 3,
            commits: 2,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 42_000,
            folded: false,
        }];
        s.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 3)]),
            borda: BTreeMap::new(),
            winner: 'A',
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
        s
    }

    #[test]
    fn run_report_names_the_winner_and_its_author() {
        let _guard = plain();
        let text = run(&state());
        assert!(text.contains("<- winner"), "{text}");
        assert!(text.contains("opus"));
        assert!(text.contains("3 files, 2 commits"));
        assert!(text.contains("winner        A"));
        assert!(!text.contains('\x1b'), "colour leaked into a plain render");
    }

    #[test]
    fn a_candidates_own_summary_is_surfaced_not_only_kept_in_run_json() {
        // Recorded on every run (`prompt::implement`'s `## SUMMARY`), but
        // `run()` used to never print it at all — silent even when the
        // summary was the one place an implementer explained itself (e.g.
        // an investigation task's findings), and readable only by opening
        // `run.json` by hand.
        let _guard = plain();
        let mut s = state();
        s.candidates[0].summary =
            "investigated 6c5e/8df3: both already merged, see talk 07fe.\nmore detail below."
                .to_owned();
        let text = run(&s);
        assert!(
            text.contains("investigated 6c5e/8df3: both already merged, see talk 07fe."),
            "{text}"
        );
    }

    #[test]
    fn a_verified_noop_run_does_not_read_as_a_failure() {
        // Same spirit as `a_mode_none_merge_does_not_read_as_landed`: a run
        // that settled without landing anything must not be misreadable as
        // the ordinary failure it is not.
        let _guard = plain();
        let mut s = state();
        s.status = RunStatus::VerifiedNoop;
        s.tally = None;
        s.candidates = vec![Candidate {
            index: 0,
            label: 'A',
            agent: "opus".to_owned(),
            branch: "magi/x/A".to_owned(),
            worktree: PathBuf::from("/wt/A"),
            summary: String::new(),
            stat: String::new(),
            files: 0,
            commits: 0,
            empty: true,
            failed: None,
            verified_noop: Some("already fixed by b32cfc4, which is on main".to_owned()),
            duration_ms: 9_000,
            folded: false,
        }];
        let text = run(&s);
        assert!(
            text.contains("agent-verified no-op"),
            "the status and the candidate flag must both say so: {text}"
        );
        assert!(
            text.contains("already fixed by b32cfc4"),
            "the evidence itself must be readable, not just the verdict: {text}"
        );
        assert!(
            !text.to_lowercase().contains("failed"),
            "a verified no-op must never read as the failure it is not: {text}"
        );
    }

    #[test]
    fn colour_is_emitted_only_when_enabled() {
        let _guard = plain();
        set_color(true);
        let coloured = run(&state());
        set_color(false);
        let plain = run(&state());
        assert!(coloured.contains('\x1b'));
        assert!(!plain.contains('\x1b'));
        assert!(coloured.len() > plain.len());
    }

    #[test]
    fn list_line_is_single_line() {
        let _guard = plain();
        let l = line(&state());
        assert_eq!(l.lines().count(), 1);
        assert!(l.contains("add retries"));
        assert!(l.contains("win A (opus)"));
    }

    #[test]
    fn an_uncontested_run_does_not_read_as_a_collapsed_panel() {
        let _guard = plain();
        let mut s = state();
        s.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 0)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 0,
            unanimous_initial: false,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: false,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some(
                "only candidate A produced a usable change; no panel was asked".to_owned(),
            ),
        });
        let text = run(&s);
        assert!(
            !text.contains("0/3"),
            "no panel sat, so the judges line must not read as one that collapsed: {text}"
        );
        assert!(!text.contains("no usable ranking"), "{text}");
        assert!(!text.contains("still split"), "{text}");
        assert!(!text.contains("BELOW QUORUM"), "{text}");
        assert!(
            text.contains("not needed"),
            "the report must say judging was skipped, not silent: {text}"
        );
        assert!(text.contains("winner        A"));
    }

    #[test]
    fn a_below_quorum_run_still_reads_as_a_collapsed_panel() {
        let _guard = plain();
        let mut s = state();
        s.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 1), ('B', 0)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 1,
            unanimous_initial: false,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: false,
            tie_break: None,
            judges: 3,
            present: 1,
            quorum: 2,
            met_quorum: false,
            uncontested: None,
        });
        let text = run(&s);
        assert!(text.contains("1/3"), "{text}");
        assert!(
            text.contains("BELOW QUORUM"),
            "a real collapse must still be flagged: {text}"
        );
        assert!(
            !text.contains("not needed"),
            "a collapsed panel must not be described as one that was never asked: {text}"
        );
    }

    #[test]
    fn a_mode_none_merge_does_not_read_as_landed() {
        let _guard = plain();
        let mut s = state();
        s.merge = Some(MergeOutcome {
            mode: crate::config::MergeMode::None,
            ok: true,
            detail: "git -C /repo merge --no-ff magi/x/A".to_owned(),
        });
        let text = run(&s);
        assert!(
            !text.contains("  ok"),
            "mode none must not be shown as a landed merge: {text}"
        );
        assert!(text.contains("not landed"), "{text}");
        assert!(
            text.contains("branch magi/x/A"),
            "the report must say what's left behind: {text}"
        );
        assert!(
            text.contains("rebase"),
            "the report must point at the hand-landing steps: {text}"
        );
        assert!(
            !text.contains("placeholder subject"),
            "the default merge style is `merge`, which never inherits a \
             placeholder subject, so the squash caveat must not appear: {text}"
        );
    }

    #[test]
    fn a_mode_none_squash_merge_warns_about_the_placeholder_subject() {
        let _guard = plain();
        let mut s = state();
        s.config.merge.style = MergeStyle::Squash;
        s.merge = Some(MergeOutcome {
            mode: crate::config::MergeMode::None,
            ok: true,
            detail: "git -C /repo merge --squash magi/x/A && git -C /repo commit -m \"add \
                      retries\""
                .to_owned(),
        });
        let text = run(&s);
        assert!(
            text.contains("placeholder subject"),
            "a squash-style manual merge must warn about the missing message: {text}"
        );
        assert!(text.contains("--squash"), "{text}");
    }

    #[test]
    fn a_ready_run_left_by_merge_mode_none_does_not_read_as_a_plain_ready() {
        let _guard = plain();
        let mut s = state();
        s.status = RunStatus::Ready;
        s.merge = Some(MergeOutcome {
            mode: crate::config::MergeMode::None,
            ok: true,
            detail: "git -C /repo merge --no-ff magi/x/A".to_owned(),
        });

        let list = line(&s);
        assert!(
            !list.contains(" ready "),
            "a mode-none run must not read as a plain ready in `magi list`: {list}"
        );
        assert!(list.contains("no-op by design"), "{list}");

        let full = run(&s);
        assert!(
            !full.contains("magi run") || !full.lines().next().unwrap().contains(" ready"),
            "the header line of `magi show` must not say plain ready either: {full}"
        );
        assert!(full.contains("no-op by design"), "{full}");
    }

    #[test]
    fn an_ordinary_ready_run_still_reads_as_ready() {
        let _guard = plain();
        let mut s = state();
        s.status = RunStatus::Ready;
        // A PR closed without merging also ends at `Ready` (see `land.rs`),
        // and unlike the honest mode-none no-op it must keep reading as a
        // plain `ready` — the label exists to flag design, not every non-merge.
        s.merge = Some(MergeOutcome {
            mode: crate::config::MergeMode::Pr,
            ok: false,
            detail: "https://example.com/pr/1 was closed without merging".to_owned(),
        });

        let list = line(&s);
        assert!(list.contains("ready"), "{list}");
        assert!(!list.contains("no-op by design"), "{list}");
    }

    #[test]
    fn the_list_line_does_not_flag_an_uncontested_run_as_short_judges() {
        let _guard = plain();
        let mut s = state();
        s.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 0)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 0,
            unanimous_initial: false,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: false,
            tie_break: None,
            judges: 0,
            present: 0,
            quorum: 0,
            met_quorum: true,
            uncontested: Some("only candidate A produced a usable change".to_owned()),
        });
        let l = line(&s);
        assert!(
            !l.contains("judges") && !l.contains("quorum"),
            "an uncontested run must not carry the same badge a short panel gets: {l}"
        );
    }

    #[test]
    fn long_instructions_are_elided() {
        let _guard = plain();
        let mut s = state();
        s.instruction = "x".repeat(200);
        assert!(line(&s).contains('…'));
    }

    #[test]
    fn a_lost_fix_report_reads_differently_from_zero_adoption() {
        let _guard = plain();
        let mut lost = state();
        lost.reviews = vec![ReviewRound {
            round: 1,
            head: "abc1234".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(FixRecord {
                agent: "opus".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: Some("timed out".to_owned()),
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 3,
            answered: 0,
            expected: 0,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        let text = run(&lost);
        assert!(text.contains("adoption report lost (timed out)"), "{text}");
        assert!(
            !text.contains("0 addressed"),
            "a lost report must never read as `0 addressed`: {text}"
        );

        let mut rejected_all = state();
        rejected_all.reviews = vec![ReviewRound {
            round: 1,
            head: "abc1234".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(FixRecord {
                agent: "opus".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: None,
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 3,
            answered: 0,
            expected: 0,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        let text2 = run(&rejected_all);
        assert!(
            text2.contains("0 addressed / 0 rejected"),
            "a round the fixer actually reported on keeps the count: {text2}"
        );
    }

    #[test]
    fn a_split_round_shows_every_seat_vote_and_the_reconsideration() {
        use crate::run::ReviewRevoteRecord;
        use crate::verdict::ReviewVote;

        let _guard = plain();
        let mut s = state();
        s.reviews = vec![ReviewRound {
            round: 1,
            head: "abc1234".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![
                ReviewRecord {
                    reviewer: 1,
                    agent: "alpha".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: Some(ReviewVote::Approve),
                    failed: None,
                    duration_ms: 0,
                },
                ReviewRecord {
                    reviewer: 2,
                    agent: "beta".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: Some(ReviewVote::Reject),
                    failed: None,
                    duration_ms: 0,
                },
            ],
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
            vote_split: true,
            reconsideration: vec![ReviewRevoteRecord {
                reviewer: 2,
                agent: "beta".to_owned(),
                vote: Some(ReviewVote::ApproveWithFindings),
                reason: "the other seat's read holds up".to_owned(),
                failed: None,
            }],
            verdict: Some(ReviewVote::ApproveWithFindings),
        }];
        let text = run(&s);
        assert!(text.contains("review-1 vote"), "{text}");
        assert!(text.contains("review-2 vote"), "{text}");
        assert!(text.contains("panel split"), "{text}");
        assert!(text.contains("reconsideration"), "{text}");
        assert!(text.contains("the other seat's read holds up"), "{text}");
    }

    #[test]
    fn an_incomplete_panel_and_a_lost_fix_report_both_stay_on_the_round_line() {
        // Two independent facts share this one line, and each arrived from a
        // different change: a seat that never answered, and a fixer whose
        // adoption report was lost. Rendering either must not shadow the
        // other, and neither may collapse into the plain `clean`/`open`
        // pair the line used to carry.
        let _guard = plain();
        let mut s = state();
        s.reviews = vec![ReviewRound {
            round: 1,
            head: "abc1234".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![
                ReviewRecord {
                    reviewer: 1,
                    agent: "alpha".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: None,
                    failed: None,
                    duration_ms: 0,
                },
                ReviewRecord {
                    reviewer: 2,
                    agent: "beta".to_owned(),
                    summary: String::new(),
                    findings: Vec::new(),
                    vote: None,
                    failed: Some("agent timed out".to_owned()),
                    duration_ms: 0,
                },
            ],
            e2e: Vec::new(),
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(FixRecord {
                agent: "opus".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: Some("timed out".to_owned()),
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 0,
            answered: 1,
            expected: 2,
            clean: false,
            progressed: true,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        let text = run(&s);
        assert!(text.contains("incomplete"), "{text}");
        assert!(text.contains("1/2 reviewers answered"), "{text}");
        assert!(text.contains("review-2: agent timed out"), "{text}");
        assert!(text.contains("adoption report lost (timed out)"), "{text}");
        assert!(
            !text.contains("clean"),
            "a round missing half its panel must never render as clean: {text}"
        );
    }

    #[test]
    fn a_build_failure_is_not_reported_as_a_test_failure() {
        let _guard = plain();
        let mut s = state();
        s.reviews = vec![ReviewRound {
            round: 1,
            head: "abc1234".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: vec![CommandOutcome {
                command: "cargo test".to_owned(),
                code: Some(1),
                output_tail: "LINK : fatal error LNK1104: cannot open file".to_owned(),
                duration_ms: 100,
                resource_blocked: false,
            }],
            verify_retried: true,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        let text = run(&s);
        assert!(text.contains("could not run"), "{text}");
        assert!(text.contains("retried once"), "{text}");
        assert!(!text.contains("e2e RED"), "{text}");
    }

    #[test]
    fn a_resource_blocked_e2e_never_reads_as_red_or_as_a_build_failure() {
        let _guard = plain();
        let mut s = state();
        s.reviews = vec![ReviewRound {
            round: 1,
            head: "abc1234".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: Vec::new(),
            e2e: vec![CommandOutcome {
                command: "(waiting for the shared build cache)".to_owned(),
                code: None,
                output_tail: "held by run x node e2e seat e2e".to_owned(),
                duration_ms: 100,
                resource_blocked: true,
            }],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        let text = run(&s);
        assert!(text.contains("shared build cache unavailable"), "{text}");
        assert!(!text.contains("e2e RED"), "{text}");
        assert!(!text.contains("build/link failure"), "{text}");
    }

    #[test]
    fn a_round_with_more_than_one_e2e_command_names_each_one() {
        // The aggregate `e2e RED` label says the round's overall verdict,
        // never which of several `verify.e2e` commands actually failed —
        // `magi show` must list each command by name, the same way it
        // already does for `gate`.
        let _guard = plain();
        let mut s = state();
        s.reviews = vec![ReviewRound {
            round: 1,
            head: "abc1234".to_owned(),
            verified_head: Some("abc1234".to_owned()),
            verified_at: Some(jiff::Timestamp::now()),
            reviews: Vec::new(),
            e2e: vec![
                CommandOutcome {
                    command: "cargo test --locked --all-targets".to_owned(),
                    code: Some(0),
                    output_tail: String::new(),
                    duration_ms: 0,
                    resource_blocked: false,
                },
                CommandOutcome {
                    command: "cargo make check".to_owned(),
                    code: Some(1),
                    output_tail: "clippy: unused import".to_owned(),
                    duration_ms: 0,
                    resource_blocked: false,
                },
            ],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: None,
            blocking: 0,
            answered: 0,
            expected: 0,
            clean: false,
            progressed: false,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];
        let text = run(&s);
        assert!(text.contains("cargo test --locked --all-targets"), "{text}");
        assert!(text.contains("cargo make check"), "{text}");
        assert!(text.contains("clippy: unused import"), "{text}");
    }

    #[test]
    fn a_declined_finding_shows_its_reason() {
        use crate::verdict::{Finding, Rejection, Severity};

        let _guard = plain();
        let mut s = state();
        s.status = RunStatus::Ready;
        s.reviews = vec![ReviewRound {
            round: 1,
            head: "deadbee".to_owned(),
            verified_head: None,
            verified_at: None,
            reviews: vec![ReviewRecord {
                reviewer: 1,
                agent: "alpha".to_owned(),
                summary: String::new(),
                findings: vec![Finding {
                    id: "R1-1-1".to_owned(),
                    severity: Severity::Major,
                    file: None,
                    line: None,
                    title: "still open".to_owned(),
                    detail: String::new(),
                }],
                vote: None,
                failed: None,
                duration_ms: 0,
            }],
            e2e: vec![CommandOutcome {
                command: "cargo test".to_owned(),
                code: Some(0),
                output_tail: String::new(),
                duration_ms: 0,
                resource_blocked: false,
            }],
            verify_retried: false,
            e2e_deferred: false,
            e2e_defer_reason: None,
            fix: Some(FixRecord {
                agent: "alpha".to_owned(),
                addressed: Vec::new(),
                rejected: vec![Rejection {
                    id: "R1-1-2".to_owned(),
                    why: "cannot be triggered from any caller".to_owned(),
                }],
                notes: String::new(),
                committed: true,
                failed: None,
                duration_ms: 0,
                continuation: None,
            }),
            blocking: 1,
            answered: 1,
            expected: 1,
            clean: false,
            progressed: true,
            vote_split: false,
            reconsideration: Vec::new(),
            verdict: None,
        }];

        let text = run(&s);
        assert!(text.contains("R1-1-2"), "{text}");
        assert!(text.contains("cannot be triggered"), "{text}");
        assert!(text.contains("still open"), "{text}");
        assert!(
            text.contains("handed off"),
            "a mergeable run with an open round must say so: {text}"
        );
    }

    #[test]
    fn a_failing_gate_command_shows_its_output() {
        let _guard = plain();
        let mut s = state();
        s.status = RunStatus::Blocked;
        s.gate = vec![CommandOutcome {
            command: "cargo make check".to_owned(),
            code: Some(101),
            output_tail: "error[E0308]: mismatched types".to_owned(),
            duration_ms: 0,
            resource_blocked: false,
        }];
        s.gate_ran = true;

        let text = run(&s);
        assert!(text.contains("mismatched types"), "{text}");
    }

    #[test]
    fn a_gate_with_no_commands_configured_shows_a_pass_not_silence() {
        let _guard = plain();
        let mut s = state();
        s.status = RunStatus::Ready;
        s.gate_ran = true;
        assert!(s.gate.is_empty());

        let text = run(&s);
        assert!(
            text.contains("gate") && text.contains("no gate commands configured"),
            "a run gated on nothing must say so, not read as if the gate never ran: {text}"
        );
    }

    #[test]
    fn a_gate_that_has_not_run_yet_shows_nothing() {
        let _guard = plain();
        let s = state();
        assert!(!s.gate_ran);
        assert!(s.gate.is_empty());

        let text = run(&s);
        assert!(
            !text.contains("no gate commands configured"),
            "an unattempted gate must not be shown as a pass: {text}"
        );
    }

    #[test]
    fn active_seats_shows_who_has_not_answered_and_how_long_is_left() {
        let _guard = plain();
        let mut s = state();
        s.seat_started("judge", "judge-2", std::time::Duration::from_secs(120), 0);
        let text = active_seats(&s, true);
        assert!(text.contains("running now"));
        assert!(text.contains("judge-2"));
        assert!(text.contains("judge"));
        assert!(!text.contains("no live daemon"), "{text}");
    }

    #[test]
    fn active_seats_flags_a_leftover_from_a_dead_process() {
        let _guard = plain();
        let mut s = state();
        s.seat_started("implement", "impl-B", std::time::Duration::from_secs(60), 0);
        let text = active_seats(&s, false);
        assert!(
            text.contains("no live daemon"),
            "a stale entry must not read as running: {text}"
        );
    }

    #[test]
    fn active_seats_is_empty_when_nothing_is_running() {
        let _guard = plain();
        assert_eq!(active_seats(&state(), true), "");
    }

    #[test]
    fn no_jobs_section_appears_when_nothing_was_ever_collected() {
        let _guard = plain();
        // The common case today (every backend but codex): silence, not a
        // clutter line repeated on every single `magi show`.
        assert!(!run(&state()).contains("background jobs"));
    }

    #[test]
    fn a_codex_roster_with_no_completed_jobs_yet_says_so_instead_of_staying_silent() {
        let _guard = plain();
        let mut s = state();
        s.config.agents.push(crate::config::AgentSpec {
            id: "codex-one".to_owned(),
            kind: crate::config::AgentKind::Codex,
            model: None,
            command: vec!["codex".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        });
        let text = run(&s);
        assert!(
            text.contains("background jobs"),
            "a run that could report this must not read the same as one that never could: \
             {text}"
        );
        assert!(text.contains("no completed command evidence yet"));
    }

    #[test]
    fn recovered_running_and_unreadable_jobs_are_told_apart() {
        let _guard = plain();
        let mut s = state();
        s.jobs = vec![
            crate::run::JobRecord {
                node: "implement".to_owned(),
                round: None,
                seat: "impl-A".to_owned(),
                id: "item49".to_owned(),
                description: "cargo test --test graph_cached_gate".to_owned(),
                checked_at: jiff::Timestamp::now(),
                status: crate::run::JobStatus::Completed,
                exit_code: Some(0),
                result_summary: "test result: 2 passed; 0 failed".to_owned(),
                source: "codex".to_owned(),
            },
            crate::run::JobRecord {
                node: "fix".to_owned(),
                round: None,
                seat: "impl-A".to_owned(),
                id: "item52".to_owned(),
                description: "cargo test --test graph_split".to_owned(),
                checked_at: jiff::Timestamp::now(),
                status: crate::run::JobStatus::Failed,
                exit_code: Some(101),
                result_summary: "test result: 1 passed; 1 failed".to_owned(),
                source: "codex".to_owned(),
            },
            crate::run::JobRecord {
                node: "fix".to_owned(),
                round: None,
                seat: "impl-A".to_owned(),
                id: "item60".to_owned(),
                description: "cargo build".to_owned(),
                checked_at: jiff::Timestamp::now(),
                status: crate::run::JobStatus::Unknown,
                exit_code: None,
                result_summary: String::new(),
                source: "codex".to_owned(),
            },
        ];
        let text = run(&s);
        assert!(text.contains("background jobs"));
        assert!(text.contains("item49"));
        assert!(text.contains("item52"));
        assert!(text.contains("item60"));
        // The three states this run actually has evidence for must read
        // differently from one another — never collapsed into a single
        // "ran" or "did not run".
        assert!(text.contains("completed"));
        assert!(text.contains("failed"));
        assert!(text.contains("unknown"));
        // Coverage limit stated once, not fabricated per seat.
        assert!(text.contains("adapter coverage"));
    }

    #[test]
    fn a_jobs_own_round_is_shown_when_known() {
        let _guard = plain();
        let mut s = state();
        s.jobs = vec![crate::run::JobRecord {
            node: "review".to_owned(),
            round: Some(2),
            seat: "review-1".to_owned(),
            id: "item9".to_owned(),
            description: "cargo test --test graph_cached_gate".to_owned(),
            checked_at: jiff::Timestamp::now(),
            status: crate::run::JobStatus::Completed,
            exit_code: Some(0),
            result_summary: "test result: 2 passed; 0 failed".to_owned(),
            source: "codex".to_owned(),
        }];
        let text = run(&s);
        assert!(
            text.contains("round 2"),
            "the round this seat's own command ran in must be visible, distinct from magi's \
             own recorded verify: {text}"
        );
    }

    #[test]
    fn stats_table_renders_without_runs() {
        let _guard = plain();
        let text = stats(&Stats::default());
        assert!(text.contains("0 total"));
        assert!(!text.contains("implementation"));
    }
}
