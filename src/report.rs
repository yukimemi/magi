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

use crate::run::{RunState, RunStatus};
use crate::stats::Stats;

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
        if t.judges > 0 {
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
        }
        if !t.met_quorum {
            let _ = writeln!(
                s,
                "  {}",
                bold(&red("BELOW QUORUM — verdict is not trustworthy"))
            );
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
        let _ = writeln!(s, "  winner        {}", bold(&green(&t.winner.to_string())));
    }

    if !state.reviews.is_empty() {
        let _ = writeln!(s, "\n{}", bold("review + verification"));
        for r in &state.reviews {
            let raised: usize = r.reviews.iter().map(|x| x.findings.len()).sum();
            let e2e = if r.e2e.is_empty() {
                dim("no e2e")
            } else if r.e2e.iter().all(|o| o.ok()) {
                green("e2e green")
            } else {
                red("e2e RED")
            };
            let _ = writeln!(
                s,
                "  round {}  {} @ {}  {raised} finding(s), {} blocking, {e2e}{}",
                r.round,
                if r.clean {
                    green("clean")
                } else {
                    yellow("open")
                },
                short(&r.head),
                r.blocking,
                r.fix.as_ref().map_or(String::new(), |f| format!(
                    "  fix: {} addressed / {} rejected{}",
                    f.addressed.len(),
                    f.rejected.len(),
                    if f.committed {
                        String::new()
                    } else {
                        red(" (NO COMMIT)")
                    }
                ))
            );
            for rec in &r.reviews {
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
        }
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
        }
    }

    if let Some(m) = &state.merge {
        let _ = writeln!(s, "\n{}", bold("merge"));
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
            "  {:<14}{:>8}{:>10}{:>11}{:>9}{:>9}",
            "reviewer", "rounds", "submitted", "adopted/rd", "precision", "unique"
        );
        for r in &stats.reviewers {
            let _ = writeln!(
                s,
                "  {:<14}{:>8}{:>10}{:>11.2}{:>8.0}%{:>8.0}%",
                r.agent,
                r.rounds,
                r.submitted,
                r.adopted_per_round(),
                r.precision(),
                r.unique_rate()
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
    use crate::run::{Candidate, RunState, Tally};
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
    fn long_instructions_are_elided() {
        let _guard = plain();
        let mut s = state();
        s.instruction = "x".repeat(200);
        assert!(line(&s).contains('…'));
    }

    #[test]
    fn stats_table_renders_without_runs() {
        let _guard = plain();
        let text = stats(&Stats::default());
        assert!(text.contains("0 total"));
        assert!(!text.contains("implementation"));
    }
}
