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

use crate::config::MergeMode;
use crate::run::{CommandOutcome, RunState, RunStatus, tail};
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
fn status_word(status: RunStatus) -> String {
    let text = format!("{status:?}").to_lowercase();
    match status {
        RunStatus::Merged => bold(&green(&text)),
        RunStatus::Ready => green(&text),
        RunStatus::Stalled => bold(&yellow(&text)),
        RunStatus::Blocked => yellow(&text),
        RunStatus::Failed => red(&text),
        _ => cyan(&text),
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
        status_word(state.status),
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

/// Full report for one run.
pub fn run(state: &RunState) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{} {}  {}",
        bold("magi run"),
        bold(&state.id),
        status_word(state.status)
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
        let flag = match (&c.failed, c.empty) {
            (Some(e), _) => red(&format!("failed: {e}")),
            (None, true) => yellow("no change"),
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
            // must not read the same as a real test failure.
            let e2e = if r.e2e.is_empty() {
                dim("no e2e")
            } else if r.e2e.iter().all(|o| o.ok()) {
                green("e2e green")
            } else if r.e2e.iter().any(CommandOutcome::build_failed) {
                yellow("e2e could not run (build/link failure)")
            } else {
                red("e2e RED")
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
                "  round {}  {} @ {}{panel}  {raised} finding(s), {} blocking, {e2e}{verdict}{}",
                r.round,
                status,
                short(&r.head),
                r.blocking,
                r.fix.as_ref().map_or(String::new(), |f| {
                    let tree = if r.progressed {
                        green("changed")
                    } else {
                        yellow("unchanged")
                    };
                    match &f.failed {
                        // Never the same shape as "N addressed / M rejected": the
                        // fixer's diff may well have landed (see the `fix` node's
                        // own event), but whether it addressed anything is
                        // unknown, not zero.
                        Some(reason) => format!(
                            "  fix: {}, tree {tree}{}",
                            yellow(&format!("adoption report lost ({reason})")),
                            if f.committed {
                                String::new()
                            } else {
                                red(" (NO COMMIT)")
                            }
                        ),
                        None => format!(
                            "  fix: {} addressed / {} rejected, tree {tree}{}",
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

    if !state.gate.is_empty() {
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
            let _ = writeln!(
                s,
                "  rebase onto {} before merging by hand, and pass an explicit \
                 commit message — a squash merge otherwise inherits the \
                 candidate's placeholder subject",
                state.base_branch
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

    if stats.e2e.rounds > 0 {
        let _ = writeln!(s, "\n{}", bold("verification"));
        let _ = writeln!(
            s,
            "  {} rounds ran e2e, {} failed, {} of those with a clean static \
             review ({:.0}% sole detections)",
            stats.e2e.rounds,
            stats.e2e.failures,
            stats.e2e.sole_detections,
            stats.e2e.sole_rate()
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
            reviews: Vec::new(),
            e2e: Vec::new(),
            verify_retried: false,
            fix: Some(FixRecord {
                agent: "opus".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: Some("timed out".to_owned()),
                duration_ms: 0,
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
            reviews: Vec::new(),
            e2e: Vec::new(),
            verify_retried: false,
            fix: Some(FixRecord {
                agent: "opus".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: None,
                duration_ms: 0,
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
            fix: Some(FixRecord {
                agent: "opus".to_owned(),
                addressed: Vec::new(),
                rejected: Vec::new(),
                notes: String::new(),
                committed: true,
                failed: Some("timed out".to_owned()),
                duration_ms: 0,
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
            reviews: Vec::new(),
            e2e: vec![CommandOutcome {
                command: "cargo test".to_owned(),
                code: Some(1),
                output_tail: "LINK : fatal error LNK1104: cannot open file".to_owned(),
                duration_ms: 100,
            }],
            verify_retried: true,
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
    fn a_declined_finding_shows_its_reason() {
        use crate::verdict::{Finding, Rejection, Severity};

        let _guard = plain();
        let mut s = state();
        s.status = RunStatus::Ready;
        s.reviews = vec![ReviewRound {
            round: 1,
            head: "deadbee".to_owned(),
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
            }],
            verify_retried: false,
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
        }];

        let text = run(&s);
        assert!(text.contains("mismatched types"), "{text}");
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
    fn stats_table_renders_without_runs() {
        let _guard = plain();
        let text = stats(&Stats::default());
        assert!(text.contains("0 total"));
        assert!(!text.contains("implementation"));
    }
}
