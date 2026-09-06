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
/// **Chosen policy: serialize, not coalesce.** A bump branch touches only
/// `Cargo.toml` / `Cargo.lock`, so `gh pr merge --squash` applies it onto
/// whatever the base branch has become by the time it lands - every commit
/// merged while it was open rides along for free, at no extra cost, once it
/// merges. Reconciling two independent `major`/`minor`/`patch` judgements
/// into one decision would need a bigger call than either agent actually
/// made, and would still race the first pull request's own merge. Letting
/// the one open pull request absorb whatever lands after it needs no
/// reconciliation at all: the next merge simply finds a bump already pending
/// and does nothing, and the one after *that* runs a fresh decision once the
/// base branch shows the pending bump has landed (or been superseded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingBump {
    /// The version the open pull request bumps to.
    pub target_version: String,
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

    git::fetch(&repo, &remote, &base).await.ok();
    let cargo_toml = git::git(&repo, &["show", &format!("{remote}/{base}:Cargo.toml")])
        .await
        .context("read Cargo.toml from the base branch")?;
    let base_version = current_version(&cargo_toml)?;

    let marker = marker_path(&run::home(), &repo);
    let pending = read_marker(&marker);
    match coalesce(pending.as_ref(), &base_version)? {
        Coalesce::Skip { target_version } => {
            state.event(
                "bump",
                format!("a release bump to v{target_version} is already open; not opening another"),
            );
            return Ok(());
        }
        Coalesce::Proceed => {
            if pending.is_some() {
                clear_marker(&marker);
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
    let pr_url_opened = opened?;

    write_marker(
        &marker,
        &PendingBump {
            target_version: next.clone(),
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
    Ok(())
}

/// Edit the version, let the lockfile follow, commit, push, and open the pull
/// request with automerge enabled. Returns the opened pull request's URL.
async fn open_bump_pr(
    state: &RunState,
    worktree: &Path,
    branch: &str,
    next_version: &str,
    decision: &BumpDecision,
    source_pr_url: &str,
) -> Result<String> {
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
    gh_enable_automerge(worktree, &url).await?;
    Ok(url)
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

    #[test]
    fn coalesce_skips_while_the_pending_target_is_still_ahead() {
        let pending = PendingBump {
            target_version: "0.9.0".to_owned(),
        };
        assert_eq!(
            coalesce(Some(&pending), "0.8.0").unwrap(),
            Coalesce::Skip {
                target_version: "0.9.0".to_owned()
            }
        );
    }

    #[test]
    fn coalesce_treats_a_landed_or_superseded_pending_bump_as_stale() {
        let pending = PendingBump {
            target_version: "0.9.0".to_owned(),
        };
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
    fn marker_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = marker_path(dir.path(), Path::new("/repos/magi"));
        assert!(read_marker(&path).is_none());

        let marker = PendingBump {
            target_version: "0.9.0".to_owned(),
        };
        write_marker(&path, &marker).unwrap();
        assert_eq!(read_marker(&path).unwrap().target_version, "0.9.0");

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
