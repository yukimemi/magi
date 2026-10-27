//! magi — a blind multi-agent implementation competition.
//!
//! One task, several agents, and a graph that decides which implementation
//! survives without a human reading the diff:
//!
//! 1. **implement** — N agents solve the same task in isolated git worktrees,
//!    unaware of each other.
//! 2. **judge** — M judges rank the candidates blind: labels `A`/`B`/`C`, a
//!    per-judge presentation order, attribution trailers stripped at write time
//!    by a `commit-msg` hook and again at presentation time.
//! 3. **deliberate** — if the first choices disagree, the judges argue, with
//!    magi as the facilitator. A facilitator made of code cannot leak an author.
//! 4. **vote** — final votes are collected one-to-one and privately, so nobody
//!    can drift toward a visible majority.
//! 5. **review** — the winner enters a bounded review + real-machine
//!    verification loop with a fixer that may reject a finding with an argument.
//! 6. **gate** — configured commands must pass before anything merges.
//!
//! Everything is recorded, so the by-products are real numbers: per-agent win
//! rates, per-reviewer precision, and how often execution caught what static
//! review missed. See [`stats`].
#![deny(missing_docs)]

pub mod advise;
pub mod agent;
pub mod already;
pub mod ask;
mod base_sync_recovery;
pub mod blind;
pub mod blockers;
pub mod bump;
pub mod cache;
pub mod chat_control;
mod chat_merge;
pub mod chat_notify;
pub mod chat_watch;
pub mod clean;
pub mod conduct;
pub mod config;
pub mod consult;
pub mod daemon;
pub mod deputy;
pub mod direct;
pub mod disk;
mod driver;
pub mod dupes;
pub mod fixer;
mod fold_evidence;
pub mod followup;
pub mod git;
pub mod github_text;
pub mod graft;
pub mod graft_report;
pub mod graph;
pub mod handover;
mod id_reserve;
pub mod land;
pub mod lang;
mod lock_file;
pub mod md;
pub mod merge_decision;
pub mod notice_delivery;
pub mod notices;
pub mod persona;
pub mod persona_icon;
mod pr_head;
pub mod pr_outcome;
pub mod pr_recovery;
pub mod proc;
pub mod prompt;
pub mod question_delivery;
pub mod queue;
pub mod rebase;
pub mod reconcile;
pub mod recovery;
pub mod refs;
pub mod release_local;
pub mod release_manifest;
pub mod release_watch;
pub mod repo_clone;
mod repo_lock;
pub mod report;
pub mod report_view;
pub mod repos;
pub mod retention;
pub mod review_repair;
pub mod rng;
pub mod run;
pub mod scrub;
mod seat_process;
pub mod settings;
pub mod stats;
mod stop_reason;
pub mod talk;
pub mod task_chat;
pub mod triage;
pub mod tui;
pub mod updater;
pub mod verdict;
pub mod waiter;
pub mod web;
pub mod worker;
pub mod worktree_setup;

pub mod choice_effect;
mod resume;

// Run environment-sensitive unit fixtures as ordinary standalone invocations,
// without mutating the concurrent test harness or inheriting worker ownership.
#[cfg(test)]
pub(crate) fn standalone_test(name: &str) -> bool {
    const CHILD: &str = "MAGI_STANDALONE_TEST";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    for key in worker::INVOCATION_ENV {
        command.env_remove(key);
    }
    let output = command
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .output()
        .expect("standalone test process");
    assert!(
        output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "standalone test {name} failed: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}
