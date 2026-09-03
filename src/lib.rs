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

pub mod agent;
pub mod ask;
pub mod blind;
pub mod chat;
pub mod config;
pub mod daemon;
pub mod git;
pub mod graph;
pub mod land;
pub mod md;
pub mod plan;
pub mod prompt;
pub mod queue;
pub mod report;
pub mod repos;
pub mod rng;
pub mod run;
pub mod stats;
pub mod tui;
pub mod updater;
pub mod verdict;
pub mod web;
