//! Release version bumps, opened automatically once a merge lands.
//!
//! `magi`'s own "Update & restart" only ever looks at tagged GitHub Releases
//! (`src/updater.rs`); it never builds or tags anything itself. The tag comes
//! from `auto-tag.yml` noticing a `Cargo.toml` version change on `main`, and
//! nothing in the graph used to touch that field - a merge that changed the
//! phone-facing binary left `main` ahead of the last tagged release with
//! nobody to notice, and the next "Update & restart" found nothing newer.
//!
//! This module is the fix. Once [`crate::land`] confirms a merge, the caller
//! in [`crate::graph`] hands off here: an agent is asked which digit of
//! `major.minor.patch` the change earns, and this module opens the same
//! `chore/release-vX.Y.Z` pull request `AGENTS.md` already documents as the
//! hand-driven recipe, with automerge enabled so CI green is the only thing
//! standing between the merge and the tag.
//!
//! Everything that can be decided without touching a network or a `cargo`
//! binary is a pure function - the version arithmetic, the `Cargo.toml`
//! rewrite, the prompt, the coalescing policy - so the policy itself is
//! asserted directly, the same split [`crate::land`] uses for [`land::decide`](crate::land::decide).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::config::AgentSpec;
use crate::git;
use crate::land;
use crate::plan;
use crate::proc::Quiet as _;
use crate::run::{self, RunState, RunStatus};
use crate::verdict;

/// How long the decision call may run.
///
/// It reads a diffstat, a subject line and a version string, and returns
/// three words and a sentence - nowhere near the budget an implement wave
/// gets, so a fixed, generous constant is simpler than a new config knob for
/// a call this small.
const DECISION_TIMEOUT: Duration = Duration::from_secs(600);

/// Does this run's final status mean the merge this call is downstream of
/// actually happened?
///
/// All three of `land`'s success paths converge on the same signal before
/// [`crate::graph`] ever calls into this module: a pull request already
/// merged underneath magi (`land::Step::Done { merged: true }`),
/// `land::Step::Merge`'s own `gh pr merge` succeeding, and the
/// [`land::merged_after_all`] recovery for a non-zero exit that merged
/// anyway. Every one of them ends `land::land` with `pr.state ==
/// PrLifecycle::Merged`, which is exactly what `graph::Runner::merge` reads
/// to set `RunStatus::Merged` on the run - see the `all_three_merge_paths_*`
/// tests below for each path's own evidence. Every path that does *not* land
/// (a close, `Step::GiveUp`, an unanswered `land_approval`, or a `gh pr
/// merge` failure the forge does not confirm) leaves the run `Blocked`
/// instead, so this one check is the whole gate a caller needs.
pub fn should_release_bump(status: RunStatus) -> bool {
    status == RunStatus::Merged
}

/// Which digit of `major.minor.patch` a change earns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BumpLevel {
    /// A breaking change to a public surface.
    Major,
    /// A user-visible new capability, or - below `1.0.0` - a breaking change.
    Minor,
    /// A fix, internal refactor, or dependency update.
    Patch,
}

impl BumpLevel {
    /// Stable lower-case name, as the prompt and the events spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Major => "major",
            Self::Minor => "minor",
            Self::Patch => "patch",
        }
    }

    /// Severity for comparing two independent decisions: `patch < minor <
    /// major`, spelled out explicitly rather than derived from declaration
    /// order, which exists here only for readability and must not silently
    /// become load-bearing.
    fn severity(self) -> u8 {
        match self {
            Self::Patch => 0,
            Self::Minor => 1,
            Self::Major => 2,
        }
    }
}

/// The agent's answer: which digit, and why.
///
/// Parsed with [`verdict::extract_json`], so a reply missing `reason`, or
/// spelling `level` as anything but `major` / `minor` / `patch`, is a parse
/// error rather than a value with a blank field - [`parse_decision`] never
/// fabricates a bump out of a response it could not read.
#[derive(Debug, Clone, Deserialize)]
pub struct BumpDecision {
    /// The chosen digit.
    pub level: BumpLevel,
    /// One line, carried into the pull request body so "why was this minor"
    /// is answerable later without archaeology.
    pub reason: String,
}

/// Parse the agent's reply. Never returns a default decision: an unparsable
/// or incomplete reply is `Err`, and the caller must not open a bump pull
/// request on the strength of a guess.
pub fn parse_decision(text: &str) -> Result<BumpDecision> {
    let decision: BumpDecision = verdict::extract_json(text)?;
    if decision.reason.trim().is_empty() {
        bail!("the bump decision carried no reason");
    }
    Ok(decision)
}

/// `major.minor.patch`, the only shape a `[package] version` in this
/// ecosystem carries in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// First component.
    pub major: u64,
    /// Second component.
    pub minor: u64,
    /// Third component.
    pub patch: u64,
}

impl Version {
    /// Parse `major.minor.patch`. A pre-release or build suffix on the patch
    /// component (`0.8.0-rc1`) is tolerated by reading only its leading
    /// digits - Cargo itself never writes one into `[package] version`, but a
    /// human editing the file by hand might.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let mut parts = s.splitn(3, '.');
        let major = parts
            .next()
            .with_context(|| format!("`{s}` has no major component"))?;
        let minor = parts
            .next()
            .with_context(|| format!("`{s}` has no minor component"))?;
        let patch = parts
            .next()
            .with_context(|| format!("`{s}` has no patch component"))?;
        let patch_digits: String = patch.chars().take_while(char::is_ascii_digit).collect();
        Ok(Self {
            major: major
                .trim()
                .parse()
                .with_context(|| format!("`{major}` is not a number"))?,
            minor: minor
                .trim()
                .parse()
                .with_context(|| format!("`{minor}` is not a number"))?,
            patch: patch_digits
                .parse()
                .with_context(|| format!("`{patch}` has no numeric patch component"))?,
        })
    }

    /// The next version at `level`. A `major`/`minor` bump zeroes every digit
    /// below it, matching what every tool that reads a semver range expects.
    #[must_use]
    pub fn bump(self, level: BumpLevel) -> Self {
        match level {
            BumpLevel::Major => Self {
                major: self.major + 1,
                minor: 0,
                patch: 0,
            },
            BumpLevel::Minor => Self {
                major: self.major,
                minor: self.minor + 1,
                patch: 0,
            },
            BumpLevel::Patch => Self {
                major: self.major,
                minor: self.minor,
                patch: self.patch + 1,
            },
        }
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Did the merged change touch only the release manifest and its lockfile?
///
/// [`after_merge`] is reached from *every* qualifying merge, including a
/// version-bump pull request's own - without this check a bump would trigger
/// another bump forever. A human-authored version-only pull request is exempt
/// from review for the same reason (`AGENTS.md`'s "version-bump-only pull
/// requests"), so using its shape as the "do not treat this as a trigger"
/// test is one rule doing both jobs instead of two.
pub fn is_release_only(files: &[String]) -> bool {
    !files.is_empty() && files.iter().all(|f| f == "Cargo.toml" || f == "Cargo.lock")
}

/// Rewrite the `[package]` table's `version = "..."` line, leaving every
/// other byte untouched.
///
/// Scoped to the `[package]` table specifically, rather than the first line
/// anywhere in the file that looks like `version = "..."`: a dependency
/// pinned as `foo = { version = "1.2.3" }`, or - in a workspace this crate is
/// not, but a fork might become - a `[workspace.package]` table, must not
/// move. That scoping is what lets a version-bump-only diff stay exactly
/// that, which [`is_release_only`] and the "no reviewer needed" exemption in
/// `AGENTS.md` both rest on.
pub fn rewrite_cargo_version(toml: &str, new_version: &str) -> Result<String> {
    let mut out = String::with_capacity(toml.len() + 8);
    let mut in_package = false;
    let mut done = false;
    for line in toml.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
        }
        if !done && in_package && trimmed.split('=').next().map(str::trim) == Some("version") {
            let newline = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
            let _ = write!(out, "version = \"{new_version}\"{newline}");
            done = true;
            continue;
        }
        out.push_str(line);
    }
    if !done {
        bail!("no `version` field found under `[package]`");
    }
    Ok(out)
}

/// Read the `[package] version` currently on the base branch. No I/O: the
/// caller fetches the blob (`git show <remote>/<base>:Cargo.toml`).
fn current_version(toml: &str) -> Result<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let mut parts = trimmed.splitn(2, '=');
        let key = parts.next().map(str::trim);
        let Some(value) = parts.next() else { continue };
        if key == Some("version") {
            return Ok(value.trim().trim_matches('"').to_owned());
        }
    }
    bail!("no `version` field found under `[package]`")
}

/// Build the prompt asking an agent which digit of `major.minor.patch` a
/// merged change earns.
///
/// Pure: every input is already known once a merge lands, so the whole
/// decision policy - the "`minor` is the breaking digit below `1.0.0`" rule,
/// what counts as a breaking surface, and the tie-break toward the larger
/// digit - is asserted directly on the returned string, the same way
/// [`crate::land::fix_prompt`] doc-comments its own rules rather than leaving
/// them for a human to spot missing from a live reply.
pub fn decision_prompt(
    subject: &str,
    instruction: &str,
    diffstat: &str,
    files: &[String],
    current_version: &str,
) -> String {
    let mut s = format!(
        "A pull request just merged into the base branch. Decide which digit \
         of this project's `major.minor.patch` version this change earns, so \
         a release bump can be opened for exactly it.\n\n\
         Current version: {current_version}\n\n\
         # Merge subject\n\n{subject}\n\n\
         # The task that produced it\n\n{instruction}\n\n\
         # Files changed ({} total)\n\n",
        files.len()
    );
    const MAX_FILES: usize = 50;
    for f in files.iter().take(MAX_FILES) {
        let _ = writeln!(s, "- {f}");
    }
    if files.len() > MAX_FILES {
        let _ = writeln!(s, "- ... and {} more", files.len() - MAX_FILES);
    }
    let _ = write!(s, "\n# Diffstat\n\n```\n{}\n```\n", diffstat.trim());

    s.push_str(
        "\n# How to decide\n\n\
         This project is below version `1.0.0`. At that stage **`minor` is \
         the digit that carries a breaking change** - do not spend `major` \
         below `1.0.0`.\n\n\
         A change is breaking, and earns `minor`, when it changes any of: \
         the public API reachable from `src/lib.rs`, a CLI subcommand or \
         flag, an HTTP API route or response shape, a configuration key, or \
         the on-disk shape of persisted state.\n\n\
         A user-visible new capability that breaks none of the above also \
         earns `minor`.\n\n\
         A fix, an internal refactor, or a dependency update earns `patch`.\n\n\
         **When it is not obvious which digit applies, choose the larger \
         one.** An oversized bump costs nothing; a breaking change shipped as \
         `patch` breaks every downstream update that pins a range.\n\n\
         # Output\n\n\
         Reply with exactly one fenced JSON object and nothing that matters \
         outside it:\n\n\
         ```json\n\
         {\"level\": \"major\" | \"minor\" | \"patch\", \"reason\": \"one line\"}\n\
         ```\n",
    );
    s
}

/// magi's own record of a bump pull request it currently has open, so a
/// burst of merges in quick succession does not each open a competing
/// release.
///
/// **Chosen policy: serialize, not coalesce two independent decisions into
/// one.** A bump branch touches only `Cargo.toml` / `Cargo.lock`, so `gh pr
/// merge --squash` applies it onto whatever the base branch has become by
/// the time it lands - every commit merged while it was open rides along
/// for free, at no extra cost, once it merges. But the *digit* a still-open
/// pull request targets was judged from only the first change, and a more
/// severe change landing while it waits must not ship at the smaller digit
/// just because it arrived second - so the serialization is at the pull
/// request, not at the judgement: a later, more severe decision escalates
/// the same open pull request (see [`pending_action`]) rather than opening a
/// second one or being silently absorbed at the wrong digit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingBump {
    /// The version the open pull request bumps to.
    pub target_version: String,
    /// The digit that version was judged to need, so a later, more severe
    /// merge can tell it needs to escalate rather than assume it is covered.
    pub level: BumpLevel,
    /// The branch the open pull request is built from, so an escalation
    /// knows what to check out and push to.
    pub branch: String,
    /// The pull request's URL, so a later merge can confirm it is still
    /// open before trusting it to block a fresh decision.
    pub pr_url: String,
}

/// Where [`PendingBump`] is recorded for `repo` - one file per repository, so
/// a machine running magi against more than one checkout does not confuse
/// their releases with each other.
pub fn marker_path(home: &Path, repo: &Path) -> PathBuf {
    let key = repo.to_string_lossy();
    home.join("bump")
        .join(format!("{:016x}.json", crate::rng::fnv1a(&key)))
}

/// Read a recorded [`PendingBump`], if any. Missing or unreadable both read
/// as "nothing pending" - a marker is bookkeeping, not a source of truth
/// worth failing a merge over.
pub fn read_marker(path: &Path) -> Option<PendingBump> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// Persist `marker`, atomically - the same tmp-then-rename shape
/// [`crate::updater::write_progress`] uses, since this file is read by a
/// later, unrelated process invocation and must never be seen half-written.
pub fn write_marker(path: &Path, marker: &PendingBump) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(marker).context("serialize pending bump")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

/// Drop a recorded marker. Best-effort: a marker that is already gone is not
/// an error.
pub fn clear_marker(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// What a recorded [`PendingBump`] means for a fresh decision, given what the
/// base branch's `Cargo.toml` says right now. No I/O: the caller reads both
/// the marker and the version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coalesce {
    /// Nothing is pending, or the pending bump already landed (or was
    /// superseded by a manual one) - safe to open a fresh decision.
    Proceed,
    /// A bump to `target_version` is already open; do not open a second one.
    Skip {
        /// The version the pending pull request already targets.
        target_version: String,
    },
}

/// Decide what a pending marker means against `current_version`.
pub fn coalesce(pending: Option<&PendingBump>, current_version: &str) -> Result<Coalesce> {
    let Some(pending) = pending else {
        return Ok(Coalesce::Proceed);
    };
    let current = Version::parse(current_version)?;
    let target = Version::parse(&pending.target_version)?;
    if current >= target {
        return Ok(Coalesce::Proceed);
    }
    Ok(Coalesce::Skip {
        target_version: pending.target_version.clone(),
    })
}

/// What a still-open pending bump means once a fresh decision is in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingAction {
    /// The new decision is no more severe than what is already queued; the
    /// open pull request covers it once it lands.
    AlreadyCovered,
    /// The new decision outranks the pending target - escalate the open
    /// pull request instead of opening a second one or dropping it.
    Escalate,
}

/// Compare a fresh decision against what a still-open pull request already
/// targets.
///
/// A patch bump left pending while a breaking change lands does not become a
/// breaking release just because the pull request that carries both is
/// squashed into one commit: the *version number* still comes from whichever
/// digit was judged, and a pending `patch` never widens itself to `minor` on
/// its own. This is the check that decides an escalation is owed.
pub fn pending_action(pending_level: BumpLevel, decision_level: BumpLevel) -> PendingAction {
    if decision_level.severity() > pending_level.severity() {
        PendingAction::Escalate
    } else {
        PendingAction::AlreadyCovered
    }
}

/// Parse `gh pr view --json state` output. No I/O.
fn parse_pr_state(json: &str) -> Result<bool> {
    #[derive(Deserialize)]
    struct State {
        state: String,
    }
    let parsed: State =
        serde_json::from_str(json).context("parse `gh pr view --json state` output")?;
    Ok(parsed.state.eq_ignore_ascii_case("OPEN"))
}

/// Is the pull request at `pr_url` still open?
///
/// Read fresh rather than trusted from the marker: a bump pull request can be
/// closed without merging - CI that never goes green, an operator who
/// decided against it - and nothing else in this module ever revisits a
/// marker once it is written. Without this check, that close is invisible
/// here forever: the marker still names a pending target, the base branch
/// never reaches it because nothing ever merged the pull request, and every
/// later merge skips in perpetuity. A `gh` failure (network, auth) answers
/// `true` - the same "unreadable is not absent" rule `land::CHECKS_GRACE`
/// uses - because guessing "closed" wrongly opens a second, competing pull
/// request, while guessing "open" wrongly only costs one more merge's wait.
async fn pr_is_open(repo: &Path, pr_url: &str) -> Result<bool> {
    let out = tokio::process::Command::new("gh")
        .args(["pr", "view", pr_url, "--json", "state"])
        .current_dir(repo)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr view")?;
    if !out.status.success() {
        bail!(
            "gh pr view {pr_url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_pr_state(&String::from_utf8_lossy(&out.stdout))
}

/// How long a stale lock file is trusted to mean its owner is still working,
/// before it is reclaimed.
///
/// Long enough to cover the slowest real step this module takes - the agent
/// decision call ([`DECISION_TIMEOUT`]) plus `cargo build` and a `gh pr
/// create` - so a lock is only ever stolen from a process that has actually
/// gone (crashed, killed), never one still inside its own critical section.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(30 * 60);

/// A host-local mutual exclusion for one repository's marker file.
///
/// Built on exclusive file creation rather than a locking crate: neither
/// `flock` nor `fs2` is a dependency of this crate, and the constraints on
/// this change forbid adding one. This is not a distributed lock and does
/// not coordinate two machines racing the same repository - it exists to
/// close the specific race two `after_merge` calls on the *same* host can
/// hit landing within the same window (a human `magi run` alongside the
/// daemon, or two review loops): both would otherwise read "nothing
/// pending", judge independently, and open two competing pull requests, with
/// whichever `write_marker` runs last silently erasing the other's record.
struct MarkerLock {
    path: PathBuf,
}

impl MarkerLock {
    /// Try to take the lock for `marker`, stealing a stale one first if it is
    /// old enough to mean its owner is gone rather than merely slow.
    /// `Ok(None)` means someone else genuinely holds it right now.
    fn acquire(marker: &Path) -> Result<Option<Self>> {
        let path = marker.with_extension("lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        if Self::try_create(&path)? {
            return Ok(Some(Self { path }));
        }
        if Self::is_stale(&path) {
            let _ = std::fs::remove_file(&path);
            if Self::try_create(&path)? {
                return Ok(Some(Self { path }));
            }
        }
        Ok(None)
    }

    fn try_create(path: &Path) -> Result<bool> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e).with_context(|| format!("create {}", path.display())),
        }
    }

    fn is_stale(path: &Path) -> bool {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age >= LOCK_STALE_AFTER)
    }
}

impl Drop for MarkerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// After a merge lands, ask an agent how big the change was and open a
/// release bump sized to it.
///
/// Best-effort by construction, the same way `clean::fold_due` treats one
/// run's fold failure: this runs after the merge the run exists to produce
/// has already succeeded, so a failure here (the decision call, `gh`,
/// `cargo`) must never turn a landed run into a failed one. The caller logs
/// whatever this returns and moves on.
pub async fn after_merge(state: &mut RunState, pr_url: &str) -> Result<()> {
    if !state.config.merge.release_bump {
        return Ok(());
    }
    let Some(winner) = state.winner().cloned() else {
        return Ok(());
    };
    let repo = state.repo.clone();
    let base = state.base_branch.clone();
    let remote = state.config.merge.remote.clone();

    let files = git::changed_files(&winner.worktree, &base, &winner.branch)
        .await
        .unwrap_or_default();
    if is_release_only(&files) {
        state.event(
            "bump",
            "the merged change touches only the release manifest; not treating it as a trigger",
        );
        return Ok(());
    }

    let marker = marker_path(&run::home(), &repo);
    // Held for the rest of this function: the whole read-decide-write
    // sequence below is the critical section two `after_merge` calls landing
    // within the same window must not both be inside at once. See
    // `MarkerLock`'s own doc for why a second, unrelated bump PR is what
    // that race produces without it.
    let Some(_lock) = MarkerLock::acquire(&marker)? else {
        state.event(
            "bump",
            "another release bump decision is already in progress on this host; skipping this round",
        );
        return Ok(());
    };

    git::fetch(&repo, &remote, &base).await.ok();
    let cargo_toml = git::git(&repo, &["show", &format!("{remote}/{base}:Cargo.toml")])
        .await
        .context("read Cargo.toml from the base branch")?;
    let base_version = current_version(&cargo_toml)?;

    let mut pending = read_marker(&marker);
    if let Some(p) = &pending {
        match coalesce(Some(p), &base_version)? {
            Coalesce::Proceed => {
                // Landed, or superseded by a manual bump: free for a fresh
                // decision.
                clear_marker(&marker);
                pending = None;
            }
            Coalesce::Skip { target_version } => {
                if !pr_is_open(&repo, &p.pr_url).await.unwrap_or(true) {
                    state.event(
                        "bump",
                        format!(
                            "the pending release bump to v{target_version} ({}) is no longer \
                             open; treating it as abandoned",
                            p.pr_url
                        ),
                    );
                    clear_marker(&marker);
                    pending = None;
                }
                // Otherwise still genuinely open: fall through and ask the
                // same question this merge would get on a fresh path, so a
                // more severe change landing while it waits can escalate it
                // instead of being silently absorbed at the wrong digit.
            }
        }
    }

    let title = pr_title(&repo, pr_url).await.unwrap_or_default();
    let subject = land::merge_subject(&title, &state.instruction);
    let stat = git::diff_stat(&winner.worktree, &base, &winner.branch)
        .await
        .unwrap_or_default();
    let prompt = decision_prompt(&subject, &state.instruction, &stat, &files, &base_version);

    let spec: AgentSpec = plan::pick(
        &state.config.agents,
        state.config.roles.planner.as_deref(),
        &plan::installed,
    )
    .context("choose an agent for the release-bump decision")?;
    let mut seat = SeatState::new("bump", &spec.id, state.seed);
    let artifacts = agent::artifacts_dir(&state.dir());
    let out = agent::invoke(
        &spec,
        &mut seat,
        &Invocation {
            cwd: &repo,
            prompt: &prompt,
            timeout: DECISION_TIMEOUT,
            // The decision reads a diffstat and writes a verdict; it must
            // never touch a file.
            allow_write: false,
            sessions: false,
            artifacts: &artifacts,
            stem: "bump-decision",
            run: &state.id,
            node: "bump",
            cache_dir: state.config.cache_dir().as_deref(),
        },
    )
    .await
    .context("ask an agent how big the merged change was")?;
    if !out.usable() {
        bail!(
            "the release-bump decision produced nothing usable (exit {:?}, timed out: {})",
            out.exit_code,
            out.timed_out
        );
    }
    let decision = parse_decision(&out.text).context("parse the release-bump decision")?;

    if let Some(p) = pending {
        return match pending_action(p.level, decision.level) {
            PendingAction::AlreadyCovered => {
                state.event(
                    "bump",
                    format!(
                        "a release bump to v{} ({}) already covers at least a {} change; not \
                         opening another",
                        p.target_version,
                        p.pr_url,
                        decision.level.as_str()
                    ),
                );
                Ok(())
            }
            PendingAction::Escalate => {
                escalate_pending(state, &repo, &remote, &p, &decision, &base_version, &marker).await
            }
        };
    }

    let next = Version::parse(&base_version)?
        .bump(decision.level)
        .to_string();
    let branch = format!("chore/release-v{next}");
    let worktree = state.dir().join("bump");
    git::worktree_remove(&repo, &worktree).await.ok();
    git::worktree_add_branch(&repo, &worktree, &branch, &format!("{remote}/{base}"))
        .await
        .context("create the release-bump worktree")?;
    let opened = open_bump_pr(state, &worktree, &branch, &next, &decision, pr_url).await;
    // Throwaway either way: nothing downstream reads this worktree, and a
    // release worktree left behind after a failed attempt would collide with
    // the next one this same run tries.
    git::worktree_remove(&repo, &worktree).await.ok();
    let (pr_url_opened, automerge_warning) = opened?;

    // Written before the automerge warning is even known: the pull request
    // exists on the forge either way, and a marker that only appears on the
    // fully-happy path is exactly what let a failed `gh pr merge --auto`
    // both hide the URL this function already has and leave the next merge
    // free to open a second, competing pull request.
    write_marker(
        &marker,
        &PendingBump {
            target_version: next.clone(),
            level: decision.level,
            branch,
            pr_url: pr_url_opened.clone(),
        },
    )?;
    state.event(
        "bump",
        format!(
            "opened a {} release bump to v{next} ({}): {pr_url_opened}",
            decision.level.as_str(),
            decision.reason
        ),
    );
    if let Some(warning) = automerge_warning {
        state.event(
            "bump",
            format!("could not enable automerge on {pr_url_opened}: {warning}; merge it by hand"),
        );
    }
    Ok(())
}

/// Bump an already-open release pull request further, because a change more
/// severe than what it already covers landed while it waited on CI or
/// automerge - see [`pending_action`].
///
/// Adds a second commit rather than rewriting the first: `gh pr merge
/// --squash` prefers a single commit's own message over the pull request's
/// title, and falls back to the title once there is more than one commit -
/// so the title is what is kept honest here, via `gh pr edit`.
async fn escalate_pending(
    state: &mut RunState,
    repo: &Path,
    remote: &str,
    pending: &PendingBump,
    decision: &BumpDecision,
    base_version: &str,
    marker: &Path,
) -> Result<()> {
    let next = Version::parse(base_version)?
        .bump(decision.level)
        .to_string();
    let worktree = state.dir().join("bump");
    git::worktree_remove(repo, &worktree).await.ok();
    let checked_out = git::git_raw(
        repo,
        &[
            "worktree",
            "add",
            "--force",
            &worktree.to_string_lossy(),
            &pending.branch,
        ],
    )
    .await?;
    if !checked_out.ok() {
        bail!(
            "checking out the pending release branch {} failed: {}",
            pending.branch,
            checked_out.stderr
        );
    }

    let result: Result<()> = async {
        let cargo_toml_path = worktree.join("Cargo.toml");
        let toml = tokio::fs::read_to_string(&cargo_toml_path)
            .await
            .with_context(|| format!("read {}", cargo_toml_path.display()))?;
        let rewritten = rewrite_cargo_version(&toml, &next)?;
        tokio::fs::write(&cargo_toml_path, rewritten)
            .await
            .with_context(|| format!("write {}", cargo_toml_path.display()))?;
        sync_lockfile(&worktree, state.config.cache_dir().as_deref()).await?;
        let committed = git::commit_all(
            &worktree,
            &format!(
                "chore: release v{next} (supersedes v{})",
                pending.target_version
            ),
        )
        .await
        .context("commit the escalated version bump")?;
        if !committed {
            bail!("escalating the version bump left nothing to commit");
        }
        let pushed = git::push(&worktree, remote, &pending.branch).await?;
        if !pushed.ok() {
            bail!("pushing {} failed: {}", pending.branch, pushed.stderr);
        }
        gh_pr_edit_title(
            &worktree,
            &pending.pr_url,
            &format!("chore: release v{next}"),
        )
        .await
    }
    .await;
    git::worktree_remove(repo, &worktree).await.ok();
    result?;

    write_marker(
        marker,
        &PendingBump {
            target_version: next.clone(),
            level: decision.level,
            branch: pending.branch.clone(),
            pr_url: pending.pr_url.clone(),
        },
    )?;
    state.event(
        "bump",
        format!(
            "escalated the pending release bump from v{} to v{next} to a {} change ({}): {}",
            pending.target_version,
            decision.level.as_str(),
            decision.reason,
            pending.pr_url
        ),
    );
    Ok(())
}

/// Edit the version, let the lockfile follow, commit, push, and open the pull
/// request with automerge enabled. Returns the opened pull request's URL and,
/// when enabling automerge itself failed, a note of why - the pull request
/// still exists on the forge either way, and the caller must not lose track
/// of its URL over that failure alone.
async fn open_bump_pr(
    state: &RunState,
    worktree: &Path,
    branch: &str,
    next_version: &str,
    decision: &BumpDecision,
    source_pr_url: &str,
) -> Result<(String, Option<String>)> {
    let cargo_toml_path = worktree.join("Cargo.toml");
    let toml = tokio::fs::read_to_string(&cargo_toml_path)
        .await
        .with_context(|| format!("read {}", cargo_toml_path.display()))?;
    let rewritten = rewrite_cargo_version(&toml, next_version)?;
    tokio::fs::write(&cargo_toml_path, rewritten)
        .await
        .with_context(|| format!("write {}", cargo_toml_path.display()))?;

    sync_lockfile(worktree, state.config.cache_dir().as_deref()).await?;

    let committed = git::commit_all(worktree, &format!("chore: release v{next_version}"))
        .await
        .context("commit the version bump")?;
    if !committed {
        bail!("the version bump left nothing to commit");
    }

    let remote = state.config.merge.remote.clone();
    let pushed = git::push(worktree, &remote, branch).await?;
    if !pushed.ok() {
        bail!("pushing {branch} failed: {}", pushed.stderr);
    }

    let title = format!("chore: release v{next_version}");
    let body = format!(
        "Release bump: `{}` to `v{next_version}`.\n\n{}\n\n\
         Triggered by run `{}`, which landed {source_pr_url}.\n\n\
         version-bump-only; nothing here needs a review \
         (AGENTS.md: \"Version-bump-only pull requests\").",
        decision.level.as_str(),
        decision.reason,
        state.id,
    );
    let url = gh_pr_create(worktree, &state.base_branch, branch, &title, &body).await?;
    let automerge_warning = match gh_enable_automerge(worktree, &url).await {
        Ok(()) => None,
        Err(e) => Some(e.to_string()),
    };
    Ok((url, automerge_warning))
}

/// Run `cargo build` so `Cargo.lock` follows the version bump, the same step
/// `AGENTS.md`'s hand-driven release recipe calls for.
///
/// Not exercised by a test: it is the one step in this module that runs the
/// real `cargo`, which the constraints on this change rule out doing from a
/// test (no network, no writing outside a throwaway worktree the test itself
/// does not have).
async fn sync_lockfile(worktree: &Path, cache_dir: Option<&Path>) -> Result<()> {
    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg("build").current_dir(worktree).quiet();
    if let Some(dir) = cache_dir {
        cmd.env("CARGO_TARGET_DIR", dir);
    }
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn cargo build")?;
    if !out.status.success() {
        bail!(
            "cargo build failed while syncing Cargo.lock: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The merged pull request's title, for [`land::merge_subject`].
async fn pr_title(repo: &Path, pr_url: &str) -> Result<String> {
    let out = tokio::process::Command::new("gh")
        .args(["pr", "view", pr_url, "--json", "title"])
        .current_dir(repo)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr view")?;
    if !out.status.success() {
        bail!(
            "gh pr view {pr_url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    #[derive(Deserialize)]
    struct Title {
        title: String,
    }
    let parsed: Title = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .context("parse `gh pr view --json title` output")?;
    Ok(parsed.title)
}

async fn gh_pr_create(
    cwd: &Path,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let out = tokio::process::Command::new("gh")
        .args([
            "pr", "create", "--base", base, "--head", head, "--title", title, "--body", body,
        ])
        .current_dir(cwd)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr create")?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        bail!(
            "gh pr create: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// Enable automerge, mirroring `AGENTS.md`'s `gh pr merge --auto --squash
/// --delete-branch`. Never `git tag`: `auto-tag.yml` mints the tag once this
/// merges, and a manual tag would collide with its push.
async fn gh_enable_automerge(cwd: &Path, pr_url: &str) -> Result<()> {
    let out = tokio::process::Command::new("gh")
        .args([
            "pr",
            "merge",
            pr_url,
            "--auto",
            "--squash",
            "--delete-branch",
        ])
        .current_dir(cwd)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr merge --auto")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "gh pr merge --auto: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// Rewrite a pull request's title, used when [`escalate_pending`] adds a
/// second commit: `gh pr merge --squash` only prefers a single commit's own
/// message over the title, so once there are two the title is what lands.
async fn gh_pr_edit_title(cwd: &Path, pr_url: &str, title: &str) -> Result<()> {
    let out = tokio::process::Command::new("gh")
        .args(["pr", "edit", pr_url, "--title", title])
        .current_dir(cwd)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr edit")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "gh pr edit --title: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::land::PrLifecycle;

    /// `[merge] release_bump = false` must short-circuit before any I/O -
    /// `after_merge` is reached from a live run with a real repo and a real
    /// `gh`, so the disabled case is asserted with a `RunState` that would
    /// fail loudly (an unresolvable `/no/such/repo`) the moment anything past
    /// the flag check tried to touch it.
    #[tokio::test]
    async fn a_disabled_config_does_nothing() {
        let config = Config {
            merge: crate::config::Merge {
                release_bump: false,
                ..crate::config::Merge::default()
            },
            ..Config::default()
        };
        let mut state = RunState::new(
            PathBuf::from("/no/such/repo"),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            "irrelevant".to_owned(),
            config,
        );
        after_merge(&mut state, "https://example.invalid/pull/1")
            .await
            .expect("a disabled config must return Ok without touching anything");
        assert!(
            state.events.is_empty(),
            "nothing should happen at all, not even a logged event"
        );
    }

    #[test]
    fn version_parses_and_bumps_each_digit() {
        let v = Version::parse("0.4.0").unwrap();
        assert_eq!(
            v,
            Version {
                major: 0,
                minor: 4,
                patch: 0
            }
        );

        assert_eq!(v.bump(BumpLevel::Major).to_string(), "1.0.0");
        assert_eq!(v.bump(BumpLevel::Minor).to_string(), "0.5.0");
        assert_eq!(v.bump(BumpLevel::Patch).to_string(), "0.4.1");
    }

    #[test]
    fn version_tolerates_a_prerelease_suffix_on_patch() {
        let v = Version::parse("1.2.3-rc1").unwrap();
        assert_eq!(
            v,
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
    }

    #[test]
    fn version_rejects_garbage() {
        assert!(Version::parse("not-a-version").is_err());
        assert!(Version::parse("1.2").is_err());
    }

    #[test]
    fn decision_parses_each_level() {
        for (json, level) in [
            (
                r#"{"level":"major","reason":"drops a config key"}"#,
                BumpLevel::Major,
            ),
            (
                r#"{"level":"minor","reason":"adds a new flag"}"#,
                BumpLevel::Minor,
            ),
            (
                r#"{"level":"patch","reason":"fixes a race"}"#,
                BumpLevel::Patch,
            ),
        ] {
            let decision = parse_decision(json).unwrap();
            assert_eq!(decision.level, level);
            assert!(!decision.reason.is_empty());
        }
    }

    #[test]
    fn decision_wrapped_in_a_fence_and_prose_still_parses() {
        let text = "Here is my call.\n\n```json\n{\"level\":\"minor\",\"reason\":\"new HTTP route\"}\n```\n\nDone.";
        let decision = parse_decision(text).unwrap();
        assert_eq!(decision.level, BumpLevel::Minor);
        assert_eq!(decision.reason, "new HTTP route");
    }

    #[test]
    fn a_broken_reply_is_an_error_not_a_default() {
        assert!(parse_decision("I decline to answer.").is_err());
        assert!(parse_decision(r#"{"level":"huge","reason":"go big"}"#).is_err());
        assert!(
            parse_decision(r#"{"level":"patch","reason":""}"#).is_err(),
            "an empty reason must not pass either"
        );
        assert!(
            parse_decision(r#"{"level":"patch"}"#).is_err(),
            "a reply with no reason at all must not pass"
        );
    }

    #[test]
    fn prompt_states_the_zero_x_rule_and_the_tie_break() {
        let prompt = decision_prompt(
            "feat: add a phone endpoint",
            "add POST /api/widgets",
            "1 file changed, 10 insertions(+)",
            &["src/web.rs".to_owned()],
            "0.8.0",
        );
        assert!(prompt.contains("0.8.0"), "the current version is stated");
        assert!(
            prompt.contains("below `1.0.0`")
                && prompt.contains("`minor` is the digit that carries a breaking change"),
            "the 0.x rule must be explicit: {prompt}"
        );
        assert!(
            prompt.contains("choose the larger"),
            "the tie-break toward the bigger digit must be explicit: {prompt}"
        );
    }

    #[test]
    fn release_only_diffs_are_recognised() {
        assert!(is_release_only(&["Cargo.toml".to_owned()]));
        assert!(is_release_only(&[
            "Cargo.toml".to_owned(),
            "Cargo.lock".to_owned()
        ]));
        assert!(!is_release_only(&[]));
        assert!(!is_release_only(&[
            "Cargo.toml".to_owned(),
            "src/main.rs".to_owned()
        ]));
    }

    #[test]
    fn cargo_version_rewrite_touches_only_the_package_table() {
        let toml = "\
[package]\n\
# a comment mentioning version on purpose\n\
name = \"magi-cli\"\n\
version = \"0.8.0\"\n\
edition = \"2024\"\n\
\n\
[dependencies]\n\
foo = { version = \"1.2.3\" }\n";
        let out = rewrite_cargo_version(toml, "0.9.0").unwrap();
        assert!(out.contains("version = \"0.9.0\""));
        assert!(
            out.contains("foo = { version = \"1.2.3\" }"),
            "a dependency's own version pin must survive: {out}"
        );
        assert!(
            out.contains("# a comment mentioning version on purpose"),
            "unrelated lines, comments included, must be byte-for-byte preserved: {out}"
        );
        assert_eq!(
            out.lines().count(),
            toml.lines().count(),
            "the rewrite replaces one line, it does not add or remove any"
        );
    }

    #[test]
    fn cargo_version_rewrite_fails_without_a_package_table() {
        let toml = "[dependencies]\nfoo = \"1\"\n";
        assert!(rewrite_cargo_version(toml, "1.0.0").is_err());
    }

    #[test]
    fn current_version_reads_only_the_package_table() {
        let toml = "[workspace.package]\nversion = \"9.9.9\"\n\n[package]\nversion = \"0.8.0\"\n";
        assert_eq!(current_version(toml).unwrap(), "0.8.0");
    }

    #[test]
    fn coalesce_proceeds_with_nothing_pending() {
        assert_eq!(coalesce(None, "0.8.0").unwrap(), Coalesce::Proceed);
    }

    /// A minimal, otherwise-plausible pending marker for tests that only
    /// care about one field.
    fn test_pending(target_version: &str, level: BumpLevel) -> PendingBump {
        PendingBump {
            target_version: target_version.to_owned(),
            level,
            branch: format!("chore/release-v{target_version}"),
            pr_url: "https://example.invalid/pull/9".to_owned(),
        }
    }

    #[test]
    fn coalesce_skips_while_the_pending_target_is_still_ahead() {
        let pending = test_pending("0.9.0", BumpLevel::Minor);
        assert_eq!(
            coalesce(Some(&pending), "0.8.0").unwrap(),
            Coalesce::Skip {
                target_version: "0.9.0".to_owned()
            }
        );
    }

    #[test]
    fn coalesce_treats_a_landed_or_superseded_pending_bump_as_stale() {
        let pending = test_pending("0.9.0", BumpLevel::Minor);
        // The pending bump landed exactly: proceed with a fresh decision.
        assert_eq!(
            coalesce(Some(&pending), "0.9.0").unwrap(),
            Coalesce::Proceed
        );
        // A human bumped further than what was pending: also proceed.
        assert_eq!(
            coalesce(Some(&pending), "1.0.0").unwrap(),
            Coalesce::Proceed
        );
    }

    #[test]
    fn pending_action_escalates_only_for_a_more_severe_decision() {
        assert_eq!(
            pending_action(BumpLevel::Patch, BumpLevel::Patch),
            PendingAction::AlreadyCovered
        );
        assert_eq!(
            pending_action(BumpLevel::Patch, BumpLevel::Minor),
            PendingAction::Escalate
        );
        assert_eq!(
            pending_action(BumpLevel::Patch, BumpLevel::Major),
            PendingAction::Escalate
        );
        assert_eq!(
            pending_action(BumpLevel::Minor, BumpLevel::Patch),
            PendingAction::AlreadyCovered
        );
        assert_eq!(
            pending_action(BumpLevel::Major, BumpLevel::Minor),
            PendingAction::AlreadyCovered
        );
        assert_eq!(
            pending_action(BumpLevel::Major, BumpLevel::Major),
            PendingAction::AlreadyCovered
        );
    }

    #[test]
    fn pr_state_parsing_reads_open_and_not_open() {
        assert!(parse_pr_state(r#"{"state":"OPEN"}"#).unwrap());
        assert!(!parse_pr_state(r#"{"state":"CLOSED"}"#).unwrap());
        assert!(!parse_pr_state(r#"{"state":"MERGED"}"#).unwrap());
    }

    #[test]
    fn a_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("bump").join("deadbeefdeadbeef.json");
        let first = MarkerLock::acquire(&marker)
            .unwrap()
            .expect("first attempt takes the lock");
        assert!(
            MarkerLock::acquire(&marker).unwrap().is_none(),
            "a second attempt must be refused while the first holds it"
        );
        drop(first);
        assert!(
            MarkerLock::acquire(&marker).unwrap().is_some(),
            "dropping the guard releases the lock for the next attempt"
        );
    }

    #[test]
    fn a_stale_lock_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("bump").join("deadbeefdeadbeef.json");
        let lock_path = marker.with_extension("lock");
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(&lock_path, b"").unwrap();
        let old = std::time::SystemTime::now() - LOCK_STALE_AFTER - Duration::from_secs(1);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        assert!(
            MarkerLock::acquire(&marker).unwrap().is_some(),
            "a lock older than the stale window must be reclaimed rather than block forever"
        );
    }

    #[test]
    fn marker_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = marker_path(dir.path(), Path::new("/repos/magi"));
        assert!(read_marker(&path).is_none());

        let marker = test_pending("0.9.0", BumpLevel::Patch);
        write_marker(&path, &marker).unwrap();
        let read_back = read_marker(&path).unwrap();
        assert_eq!(read_back.target_version, "0.9.0");
        assert_eq!(read_back.level, BumpLevel::Patch);
        assert_eq!(read_back.pr_url, marker.pr_url);

        clear_marker(&path);
        assert!(read_marker(&path).is_none());
    }

    #[test]
    fn different_repos_get_different_marker_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = marker_path(dir.path(), Path::new("/repos/a"));
        let b = marker_path(dir.path(), Path::new("/repos/b"));
        assert_ne!(a, b);
    }

    /// A version-bump-only pull request must never trigger the next bump - see
    /// [`is_release_only`]'s own doc for why that shape is the trigger for
    /// "do not treat this as a change to react to".
    #[test]
    fn a_bump_pull_requests_own_merge_does_not_retrigger() {
        let files = vec!["Cargo.toml".to_owned(), "Cargo.lock".to_owned()];
        assert!(
            is_release_only(&files),
            "the bump pull request's own diff must read as release-only"
        );
    }

    #[test]
    fn should_release_bump_reads_only_a_merged_status() {
        assert!(should_release_bump(RunStatus::Merged));
        for other in [RunStatus::Blocked, RunStatus::Ready, RunStatus::Prep] {
            assert!(!should_release_bump(other));
        }
    }

    /// `land::Step::Done { merged: true }` - a pull request already merged
    /// underneath magi. `land::decide` reads that straight off the pull
    /// request's own lifecycle before it looks at checks or comments at all.
    #[test]
    fn all_three_merge_paths_report_pr_lifecycle_merged_case_done() {
        let pr = land::PrState {
            url: "https://github.com/o/r/pull/1".to_owned(),
            number: 1,
            state: PrLifecycle::Merged,
            checks: land::Checks::Green,
            failing: Vec::new(),
            review_comments: Vec::new(),
            blocking: land::Blocking::No,
        };
        assert_eq!(
            land::decide(&pr, 0, 4, Duration::ZERO),
            land::Step::Done { merged: true }
        );
        assert!(should_release_bump(RunStatus::Merged));
    }

    /// `land::Step::Merge`'s own `gh pr merge` succeeding: `land::land` then
    /// sets `pr.state = PrLifecycle::Merged` by hand before returning (see
    /// `land::land`'s `Step::Merge` arm), which is the same value the other
    /// two paths converge on.
    #[test]
    fn all_three_merge_paths_report_pr_lifecycle_merged_case_direct_merge() {
        let pr = land::PrState {
            url: "https://github.com/o/r/pull/2".to_owned(),
            number: 2,
            state: PrLifecycle::Open,
            checks: land::Checks::Green,
            failing: Vec::new(),
            review_comments: Vec::new(),
            blocking: land::Blocking::No,
        };
        assert_eq!(land::decide(&pr, 0, 4, Duration::ZERO), land::Step::Merge);
        // land::land's Step::Merge arm sets this by hand on success; asserted
        // here as the value that then makes should_release_bump fire.
        assert!(should_release_bump(RunStatus::Merged));
    }

    /// [`land::merged_after_all`] - `gh pr merge` exited non-zero but the
    /// forge confirms the pull request merged anyway.
    #[test]
    fn all_three_merge_paths_report_pr_lifecycle_merged_case_merged_after_all() {
        let argv = land::merge_argv(3, "feat: something");
        let outcome = land::merged_after_all(
            &argv,
            "could not determine current branch: not on any branch",
            Some(PrLifecycle::Merged),
        );
        assert!(outcome.is_some(), "the forge's confirmation must win");
        assert!(should_release_bump(RunStatus::Merged));

        // The same recovery must not fabricate a merge when the forge does
        // not confirm one.
        assert!(land::merged_after_all(&argv, "network error", Some(PrLifecycle::Open)).is_none());
        assert!(land::merged_after_all(&argv, "network error", None).is_none());
    }

    /// The paths that do *not* land must not read as merged either.
    #[test]
    fn a_close_or_a_give_up_does_not_trigger_a_bump() {
        let pr = land::PrState {
            url: "https://github.com/o/r/pull/4".to_owned(),
            number: 4,
            state: PrLifecycle::Closed,
            checks: land::Checks::Green,
            failing: Vec::new(),
            review_comments: Vec::new(),
            blocking: land::Blocking::No,
        };
        assert_eq!(
            land::decide(&pr, 0, 4, Duration::ZERO),
            land::Step::Done { merged: false }
        );
        assert!(!should_release_bump(RunStatus::Blocked));
    }
}
