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

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::agent::{self, Invocation, SeatState};
use crate::ask;
use crate::config::AgentSpec;
use crate::git;
use crate::land;
use crate::notices::{Link, Notice, Notices};
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

/// Rewrite `table`'s `version = "..."` line, leaving every other byte
/// untouched.
///
/// Scoped to the named table specifically, rather than the first line
/// anywhere in the file that looks like `version = "..."`: a dependency
/// pinned as `foo = { version = "1.2.3" }` must never move, and neither must
/// the *other* of `[package]` / `[workspace.package]` when only one of them
/// is the one being bumped. That scoping is what lets a version-bump-only
/// diff stay exactly that, which [`is_release_only`] and the "no reviewer
/// needed" exemption in `AGENTS.md` both rest on.
fn rewrite_table_version(toml: &str, table: &str, new_version: &str) -> Result<String> {
    let mut out = String::with_capacity(toml.len() + 8);
    let mut in_table = false;
    let mut done = false;
    for line in toml.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_table = trimmed == table;
        }
        if !done && in_table && trimmed.split('=').next().map(str::trim) == Some("version") {
            let newline = if line.ends_with("\r\n") { "\r\n" } else { "\n" };
            let _ = write!(out, "version = \"{new_version}\"{newline}");
            done = true;
            continue;
        }
        out.push_str(line);
    }
    if !done {
        bail!("no `version` field found under `{table}`");
    }
    Ok(out)
}

/// Truncate `s` at the first `#` that is not inside a quoted string, the
/// same rule a TOML parser uses to tell a comment from a literal `#` inside
/// a value. Used only on table-header lines here (`[workspace.dependencies]
/// # internal pins` is valid TOML), so header comparisons don't miss a
/// section just because it carries a trailing comment.
fn strip_trailing_comment(s: &str) -> &str {
    let mut in_string = false;
    let mut quote = '"';
    for (i, c) in s.char_indices() {
        if in_string {
            if c == quote {
                in_string = false;
            }
        } else if c == '"' || c == '\'' {
            in_string = true;
            quote = c;
        } else if c == '#' {
            return s[..i].trim_end();
        }
    }
    s
}

/// Strip a single layer of matching quotes from a TOML key, so a
/// `"name" = { .. }` or `'name' = { .. }` entry compares equal to the plain
/// `name` the `toml` crate itself hands back for the same key - Cargo
/// manifests almost never quote a dependency key (bare keys already allow
/// `-`), but it is legal TOML and costs nothing extra to normalise.
fn unquote_key(key: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = key.strip_prefix(quote).and_then(|k| k.strip_suffix(quote)) {
            return inner;
        }
    }
    key
}

/// Pull just the `[workspace.dependencies]` table - flat entries and any
/// `[workspace.dependencies.<name>]` sub-tables - out of a full manifest and
/// rewrite its headers to `[dependencies]` / `[dependencies.<name>]`, so the
/// fragment parses standalone as TOML.
///
/// This is what keeps [`internal_pin_names`] and [`verify_pins_rewritten`]
/// from imposing a whole-file strict-TOML precondition on every workspace
/// manifest this rewrite runs against: only the dependency table itself has
/// to parse, not any other section a hand-edited `Cargo.toml` might carry in
/// a shape this crate's pinned `toml` version does not accept. Returns an
/// empty string when the manifest has no such table at all.
fn extract_workspace_dependencies_fragment(toml: &str) -> String {
    let mut fragment = String::new();
    let mut capturing = false;
    for line in toml.split_inclusive('\n') {
        let header = strip_trailing_comment(line.trim());
        if header.starts_with('[') {
            if header == "[workspace.dependencies]" {
                capturing = true;
                fragment.push_str("[dependencies]\n");
            } else if let Some(name) = header
                .strip_prefix("[workspace.dependencies.")
                .and_then(|rest| rest.strip_suffix(']'))
            {
                capturing = true;
                let _ = writeln!(fragment, "[dependencies.{name}]");
            } else {
                capturing = false;
            }
            continue;
        }
        if capturing {
            fragment.push_str(line);
        }
    }
    fragment
}

/// Names of `[workspace.dependencies]` entries that name an internal member
/// by *both* `path` and `version` - the shape `AGENTS.md`'s own "internal
/// version pin" guidance recommends for a published member that another
/// workspace member depends on. Parsed with the `toml` crate (already a
/// dependency, used read-only here) rather than by scanning for the word
/// `version`, so an ordinary `[dependencies]` entry in some other table, or a
/// `[workspace.dependencies]` entry that carries only one of the two keys,
/// never enters the candidate set:
///
/// - `path` only (no `version`): an unpublished, workspace-only member - not
///   ours to touch.
/// - `version` only (no `path`): an external crates.io dependency - not ours
///   to touch either.
///
/// Returns an empty list, not an error, when the manifest has no
/// `[workspace.dependencies]` table at all - the common single-crate shape -
/// so [`rewrite_cargo_version`]'s existing `[package]` path is unaffected.
fn internal_pin_names(toml: &str) -> Result<Vec<String>> {
    let fragment = extract_workspace_dependencies_fragment(toml);
    if fragment.trim().is_empty() {
        return Ok(Vec::new());
    }
    let value: toml::Value =
        toml::from_str(&fragment).context("failed to parse `[workspace.dependencies]` as TOML")?;
    let mut names: Vec<String> = value
        .get("dependencies")
        .and_then(|d| d.as_table())
        .into_iter()
        .flatten()
        .filter(|(_, dep)| {
            dep.as_table()
                .is_some_and(|t| t.contains_key("path") && t.contains_key("version"))
        })
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    Ok(names)
}

/// Rewrite the quoted string value of `key = "..."` (or `key = '...'`, a
/// literal string) on `line`, wherever it appears - once for an inline table
/// (`name = { path = "..", version = ".." }`, either key order) and once for
/// a dotted `[workspace.dependencies.name]` table's own `version = ".."`
/// line. The replacement is written back between the *same* quote
/// characters the line already used, so a literal-string pin stays a
/// literal string. Returns `None` when `key` is not present on this line as
/// a `key = "value"` pair, which the caller treats as "could not rewrite
/// this one" rather than silently leaving it unbumped.
fn rewrite_quoted_field(line: &str, key: &str, new_value: &str) -> Option<String> {
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(key) {
        let key_start = search_from + rel;
        let before_ok = key_start == 0 || !is_ident(line[..key_start].chars().next_back().unwrap());
        let eq_pos = key_start
            + key.len()
            + (line[key_start + key.len()..].len()
                - line[key_start + key.len()..].trim_start().len());
        if before_ok && line[eq_pos..].starts_with('=') {
            let after_eq = &line[eq_pos + 1..];
            let ws_len = after_eq.len() - after_eq.trim_start().len();
            let quote_pos = eq_pos + 1 + ws_len;
            if let Some(quote_char) = line[quote_pos..]
                .chars()
                .next()
                .filter(|c| *c == '"' || *c == '\'')
            {
                let value_begin = quote_pos + quote_char.len_utf8();
                if let Some(end_rel) = line[value_begin..].find(quote_char) {
                    let value_end = value_begin + end_rel;
                    let mut out = String::with_capacity(line.len());
                    out.push_str(&line[..value_begin]);
                    out.push_str(new_value);
                    out.push_str(&line[value_end..]);
                    return Some(out);
                }
            }
        }
        search_from = key_start + key.len();
    }
    None
}

/// Rewrite the `version` pin of each `[workspace.dependencies]` entry named
/// in `names` to `new_version`, in every shape Cargo accepts: the inline
/// table (`name = { path = "..", version = "old" }`), the dotted-key form
/// under the flat table (`name.path = ".."` / `name.version = "old"` as two
/// separate lines), and the dotted sub-table (`[workspace.dependencies.
/// name]` followed by its own `version = "old"` line). Every other byte,
/// including entries not in `names`, is untouched - same discipline as
/// [`rewrite_table_version`].
///
/// Bails rather than silently leaving a pin unbumped when a named entry's
/// `version` field cannot be located on a single line this way (for example
/// an inline table split across lines): a bump pull request that leaves a
/// stale internal pin behind is exactly the failure this exists to prevent,
/// so an unrepresentable form must stop the bump, not ship it half-done.
fn rewrite_workspace_dependency_pins(
    toml: &str,
    names: &[String],
    new_version: &str,
) -> Result<String> {
    if names.is_empty() {
        return Ok(toml.to_owned());
    }
    let mut out = String::with_capacity(toml.len() + names.len() * 8);
    let mut in_flat_table = false;
    let mut in_named_table: Option<String> = None;
    let mut rewritten: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in toml.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            let header = strip_trailing_comment(trimmed);
            in_flat_table = header == "[workspace.dependencies]";
            in_named_table = header
                .strip_prefix("[workspace.dependencies.")
                .and_then(|rest| rest.strip_suffix(']'))
                .map(str::to_owned);
            out.push_str(line);
            continue;
        }

        let key = trimmed.split('=').next().map(str::trim).map(unquote_key);

        if in_flat_table {
            // The inline-table form: `name = { path = "..", version = ".." }`.
            if let Some(name) = key.and_then(|k| names.iter().find(|n| n.as_str() == k)) {
                if let Some(rewritten_line) = rewrite_quoted_field(line, "version", new_version) {
                    rewritten.insert(name.clone());
                    out.push_str(&rewritten_line);
                    continue;
                }
            } else if let Some(name) = key.and_then(|k| {
                k.strip_suffix(".version")
                    .and_then(|prefix| names.iter().find(|n| n.as_str() == prefix))
            }) {
                // The dotted-key form: `name.path = ".."` / `name.version =
                // ".."` as two separate lines directly under
                // `[workspace.dependencies]`, with no inline table and no
                // `[workspace.dependencies.name]` sub-header either.
                if let Some(rewritten_line) = rewrite_quoted_field(line, "version", new_version) {
                    rewritten.insert(name.clone());
                    out.push_str(&rewritten_line);
                    continue;
                }
            }
        } else if let Some(table_name) = &in_named_table {
            if key == Some("version") && names.iter().any(|n| n == table_name) {
                if let Some(rewritten_line) = rewrite_quoted_field(line, "version", new_version) {
                    rewritten.insert(table_name.clone());
                    out.push_str(&rewritten_line);
                    continue;
                }
            }
        }

        out.push_str(line);
    }

    for name in names {
        if !rewritten.contains(name) {
            bail!(
                "could not rewrite the `[workspace.dependencies]` version pin \
                 for `{name}` - its `version` field was not found in a shape \
                 this rewrite understands"
            );
        }
    }
    Ok(out)
}

/// Confirm every entry in `names` now reads `new_version` under
/// `[workspace.dependencies]`, by reparsing the rewritten manifest rather
/// than trusting the line-based rewrite's own bookkeeping. The safety net
/// this run-time failure on the pending release bump is guarding against
/// happened once already: `kanadehq/kanade`'s pin was left stale and its own
/// `internal_pins_match_the_workspace_version` regression test caught it in
/// CI, after the bump pull request had already opened.
fn verify_pins_rewritten(toml: &str, names: &[String], new_version: &str) -> Result<()> {
    let fragment = extract_workspace_dependencies_fragment(toml);
    let value: toml::Value = toml::from_str(&fragment)
        .context("rewritten `[workspace.dependencies]` failed to parse")?;
    let deps = value.get("dependencies").and_then(|d| d.as_table());
    for name in names {
        let actual = deps
            .and_then(|d| d.get(name))
            .and_then(|dep| dep.as_table())
            .and_then(|t| t.get("version"))
            .and_then(|v| v.as_str());
        if actual != Some(new_version) {
            bail!(
                "the `[workspace.dependencies]` version pin for `{name}` did \
                 not end up at `{new_version}` after the rewrite"
            );
        }
    }
    Ok(())
}

/// Rewrite the release version, wherever this manifest actually declares it.
///
/// A single crate carries its version under `[package]`. A workspace root
/// with no crate of its own - `[workspace] members = [...]` and nothing
/// else - carries it under `[workspace.package]` instead, and `[package]`
/// does not exist there at all. `[package]` is tried first because it is the
/// far more common shape and the one every existing bump so far has hit;
/// `[workspace.package]` is the fallback for the shape that never worked
/// before this.
///
/// A workspace manifest can also carry a *second*, independent version
/// literal per internal member: `[workspace.dependencies].<name>` entries
/// that reference a published member by both `path` and `version`, exactly
/// the shape `AGENTS.md` documents as the canonical place to centralize that
/// pin. Cargo does not derive that literal from `[workspace.package]
/// version` - nothing does - so once the table version is rewritten, every
/// such pin is rewritten to match in the same edit, and the result is
/// reparsed to confirm it before this function returns. An entry with only
/// `path` (an unpublished workspace-only member) or only `version` (an
/// external dependency) is left untouched, as is a plain single-crate
/// manifest with no `[workspace.dependencies]` table at all.
pub fn rewrite_cargo_version(toml: &str, new_version: &str) -> Result<String> {
    let rewritten = rewrite_table_version(toml, "[package]", new_version)
        .or_else(|_| rewrite_table_version(toml, "[workspace.package]", new_version))
        .context("no `version` field found under `[package]` or `[workspace.package]`")?;

    let pin_names = internal_pin_names(&rewritten)?;
    if pin_names.is_empty() {
        return Ok(rewritten);
    }

    let rewritten = rewrite_workspace_dependency_pins(&rewritten, &pin_names, new_version)?;
    verify_pins_rewritten(&rewritten, &pin_names, new_version)?;
    Ok(rewritten)
}

/// Find `table`'s `version` field, if it has one. No I/O.
fn version_in_table(toml: &str, table: &str) -> Option<String> {
    let mut in_table = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_table = trimmed == table;
            continue;
        }
        if !in_table {
            continue;
        }
        let mut parts = trimmed.splitn(2, '=');
        let key = parts.next().map(str::trim);
        let Some(value) = parts.next() else {
            continue;
        };
        if key == Some("version") {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Read the version currently on the base branch, from `[package]` if it has
/// one, else from `[workspace.package]` - see [`rewrite_cargo_version`] for
/// why both exist and which wins. No I/O: the caller fetches the blob (`git
/// show <remote>/<base>:Cargo.toml`).
fn current_version(toml: &str) -> Result<String> {
    version_in_table(toml, "[package]")
        .or_else(|| version_in_table(toml, "[workspace.package]"))
        .context("no `version` field found under `[package]` or `[workspace.package]`")
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
    // The reason is pasted into the release pull request's body.
    let _ = write!(
        s,
        "\n{}\n\nThe `reason` goes into a GitHub pull request body, so write it \
         in English.\n",
        crate::prompt::GITHUB_ENGLISH_HEADING
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

/// How often a blocked caller checks whether [`MarkerLock`] has freed up.
const LOCK_POLL: Duration = Duration::from_secs(5);

/// How long a caller waits for a contended lock before giving up on this
/// merge's own judgement entirely.
///
/// A first version of this gate gave up the instant the lock was taken,
/// which meant a change landing while another host's decision call was
/// still running was never judged at all - not even recorded as pending,
/// not escalated later, just dropped. The lock is only ever held for one
/// `after_merge` call, so waiting past it is what lets that call's own
/// decision reach [`pending_action`] against a marker the other side just
/// finished writing, instead of finding nothing to check against. Set just
/// under [`LOCK_STALE_AFTER`]: a lock still held this long after that point
/// is reclaimed as abandoned rather than waited on further.
const LOCK_WAIT_CEILING: Duration = Duration::from_secs(25 * 60);

/// Wait for [`MarkerLock`] to free up, polling rather than blocking forever.
/// `Ok(None)` means the ceiling passed with the lock still held.
async fn wait_for_marker_lock(marker: &Path) -> Result<Option<MarkerLock>> {
    wait_for_marker_lock_with(marker, LOCK_POLL, LOCK_WAIT_CEILING).await
}

/// [`wait_for_marker_lock`] with the poll interval and ceiling as parameters,
/// so the retry behaviour is testable without a test actually waiting out
/// [`LOCK_WAIT_CEILING`].
async fn wait_for_marker_lock_with(
    marker: &Path,
    poll: Duration,
    ceiling: Duration,
) -> Result<Option<MarkerLock>> {
    let mut waited = Duration::ZERO;
    loop {
        if let Some(lock) = MarkerLock::acquire(marker)? {
            return Ok(Some(lock));
        }
        if waited >= ceiling {
            return Ok(None);
        }
        tokio::time::sleep(poll).await;
        waited += poll;
    }
}

/// Which digit differs between `from` and `to`? `None` when they are equal.
///
/// Used to recover the level a pull request found by [`find_open_release_pr`]
/// was judged at: the forge has the resulting version (in the branch name and
/// the title) but not the digit an agent chose to get there, and this is the
/// one other host-independent fact every host can compute the same way from
/// it.
fn level_between(from: Version, to: Version) -> Option<BumpLevel> {
    if to.major != from.major {
        Some(BumpLevel::Major)
    } else if to.minor != from.minor {
        Some(BumpLevel::Minor)
    } else if to.patch != from.patch {
        Some(BumpLevel::Patch)
    } else {
        None
    }
}

/// Parse `gh pr list --state open --json url,headRefName` output, returning
/// the first pull request whose branch is one of this module's own. No I/O.
fn parse_open_release_pr(json: &str) -> Result<Option<(String, String)>> {
    #[derive(Deserialize)]
    struct Pr {
        url: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
    }
    let list: Vec<Pr> =
        serde_json::from_str(json).context("parse `gh pr list --json url,headRefName` output")?;
    Ok(list
        .into_iter()
        .find(|p| p.head_ref_name.starts_with("chore/release-v"))
        .map(|p| (p.head_ref_name, p.url)))
}

/// Every open pull request on one of this module's own branches, as
/// `(branch, url)`. No I/O.
pub(crate) fn parse_open_release_prs(json: &str) -> Result<Vec<(String, String)>> {
    #[derive(Deserialize)]
    struct Pr {
        url: String,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
    }
    let list: Vec<Pr> =
        serde_json::from_str(json).context("parse `gh pr list --json url,headRefName` output")?;
    Ok(list
        .into_iter()
        .filter(|p| p.head_ref_name.starts_with("chore/release-v"))
        .map(|p| (p.head_ref_name, p.url))
        .collect())
}

/// [`find_open_release_pr`], but every match rather than the first.
pub(crate) async fn list_open_release_prs(repo: &Path) -> Result<Vec<(String, String)>> {
    let out = tokio::process::Command::new("gh")
        .args([
            "pr",
            "list",
            "--state",
            "open",
            "--limit",
            "100",
            "--json",
            "url,headRefName",
        ])
        .current_dir(repo)
        .env_remove("GH_REPO")
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr list")?;
    if !out.status.success() {
        bail!(
            "gh pr list: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_open_release_prs(&String::from_utf8_lossy(&out.stdout))
}

/// Ask the forge directly whether a release bump is already open, for a host
/// that has never seen it.
///
/// [`MarkerLock`] and the marker file only ever coordinate *this* host - a
/// marker written on one machine is not visible to `run::home()` on another,
/// so two hosts landing runs against the same repository at the same time
/// can each read "nothing pending" and open a competing pull request no
/// local lock can see. `gh pr list` is the one place every host actually
/// shares a view, so it is consulted whenever this host's own marker says
/// there is nothing pending, before a fresh decision is allowed to open a
/// second pull request. This narrows the race to the gap between this call
/// and whichever host's `gh pr create` lands first - it does not close it -
/// because turning that into a real distributed lock would need coordination
/// this crate has no dependency for.
async fn find_open_release_pr(repo: &Path) -> Result<Option<(String, String)>> {
    let out = tokio::process::Command::new("gh")
        .args(["pr", "list", "--state", "open", "--json", "url,headRefName"])
        .current_dir(repo)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr list")?;
    if !out.status.success() {
        bail!(
            "gh pr list: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_open_release_pr(&String::from_utf8_lossy(&out.stdout))
}

/// After a merge lands, ask an agent how big the change was and open a
/// release bump sized to it.
///
/// Best-effort by construction, the same way `clean::fold_due` treats one
/// run's fold failure: this runs after the merge the run exists to produce
/// has already succeeded, so a failure here (the decision call, `gh`,
/// `cargo`) must never turn a landed run into a failed one. The caller logs
/// whatever this returns and moves on.
///
/// An `Err` is a bump that was tried and failed, never one that was not
/// eligible (every such case returns `Ok` after an event). When it fails
/// before any release pull request exists, the operator is notified here, so
/// the caller only has to record the event: nobody would otherwise learn that
/// a release silently never happened. A failure with a pull request in hand
/// goes through [`report_problem`] with its URL instead, and is not repeated.
/// A failure while a pending release pull request is in play (the decision or
/// an escalation of it) is notified here too, naming that pull request: the
/// escalation path itself never raises one, so suppressing it would leave the
/// operator with nothing.
pub async fn after_merge(state: &mut RunState, pr_url: &str) -> Result<()> {
    after_merge_at(state, pr_url, None, None).await
}

/// What one attempt had settled by the time it failed.
#[derive(Debug, Default)]
struct Progress {
    /// The target version, once the decision had been made.
    version: Option<String>,
    /// A release pull request already pending when this attempt started.
    pending_pr: Option<String>,
}

/// The `(version, reason)` to notify with for a failed attempt. The reason is
/// the first line of the full error chain, so the operator can tell a stale
/// branch from a build failure. A pull request this attempt opened never reaches here: past
/// `gh pr create` nothing returns `Err`, and automerge trouble goes through
/// [`report_problem`] itself.
fn notice_for_failure(progress: &Progress, err: &anyhow::Error) -> (Option<String>, String) {
    let chain = format!("{err:#}");
    let first = chain.lines().next().unwrap_or_default().trim();
    (progress.version.clone(), first.to_owned())
}

/// Does the base branch carry a `Cargo.toml` at its root? Release bumps read
/// and rewrite that file, so its absence means "not a Rust repository", which
/// is not a fault. `ls-tree` rather than `cat-file -e`, so an unresolvable
/// ref (a failed fetch, a misconfigured base) stays an error instead of being
/// reported as a non-Rust repository.
async fn base_has_cargo_toml(repo: &Path, remote: &str, base: &str) -> Result<bool> {
    let out = git::git(
        repo,
        &[
            "ls-tree",
            "--name-only",
            &format!("{remote}/{base}"),
            "--",
            "Cargo.toml",
        ],
    )
    .await
    .context("look for Cargo.toml on the base branch")?;
    Ok(!out.trim().is_empty())
}

/// [`after_merge`] with an optional magi home, so tests can point the
/// marker and its lock at a scratch directory.
/// `store` is where a notice is raised; `None` is the operator's own.
async fn after_merge_at(
    state: &mut RunState,
    pr_url: &str,
    home: Option<&Path>,
    store: Option<&Notices>,
) -> Result<()> {
    let mut progress = Progress::default();
    let result = after_merge_inner(state, pr_url, home, &mut progress).await;
    if let Err(e) = &result {
        let (version, reason) = notice_for_failure(&progress, e);
        let pending = progress.pending_pr.as_deref();
        let store = store.cloned().unwrap_or_else(Notices::open);
        report_problem_in(
            state,
            &store,
            pending,
            version.as_deref(),
            &reason,
            pending.is_some(),
        )
        .await;
    }
    result
}

async fn after_merge_inner(
    state: &mut RunState,
    pr_url: &str,
    home: Option<&Path>,
    progress: &mut Progress,
) -> Result<()> {
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

    // Outside the lock, and before anything that assumes a Rust manifest.
    // Fetch first so a stale remote-tracking ref cannot misjudge the base.
    git::fetch(&repo, &remote, &base).await.ok();
    if !base_has_cargo_toml(&repo, &remote, &base).await? {
        state.event(
            "bump",
            "release bump: no Cargo.toml on the base branch; release bumps are Rust-only, skipping",
        );
        return Ok(());
    }

    let marker = marker_path(&home.map_or_else(run::home, Path::to_path_buf), &repo);
    // Held for the rest of this function: the whole read-decide-write
    // sequence below is the critical section two `after_merge` calls landing
    // within the same window must not both be inside at once. See
    // `MarkerLock`'s own doc for why a second, unrelated bump PR is what
    // that race produces without it, and `wait_for_marker_lock`'s for why
    // this waits rather than giving up the instant it is contended.
    let Some(_lock) = wait_for_marker_lock(&marker).await? else {
        state.event(
            "bump",
            "another release bump decision held the lock past the wait ceiling; skipping this round",
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

    if pending.is_none() {
        // This host's own marker has nothing to say - check the forge itself
        // before trusting that to mean a fresh pull request is safe to open.
        // See `find_open_release_pr`'s own doc for what this does and does
        // not close.
        if let Ok(Some((branch, url))) = find_open_release_pr(&repo).await
            && let Some(target) = branch
                .strip_prefix("chore/release-v")
                .and_then(|v| Version::parse(v).ok())
        {
            let base_parsed = Version::parse(&base_version)?;
            if target > base_parsed
                && let Some(level) = level_between(base_parsed, target)
            {
                let adopted = PendingBump {
                    target_version: target.to_string(),
                    level,
                    branch,
                    pr_url: url,
                };
                // Best-effort: worst case this host asks the forge again
                // next time instead of finding its own record of it.
                let _ = write_marker(&marker, &adopted);
                pending = Some(adopted);
            }
        }
    }

    progress.pending_pr = pending.as_ref().map(|p| p.pr_url.clone());
    let title = pr_title(&repo, pr_url).await.unwrap_or_default();
    let subject = land::merge_subject(
        crate::graph::landing_title(state, &title),
        &crate::graph::landing_subject_source(state),
    );
    let stat = git::diff_stat(&winner.worktree, &base, &winner.branch)
        .await
        .unwrap_or_default();
    let prompt = decision_prompt(&subject, &state.instruction, &stat, &files, &base_version);

    // No dedicated role for this one-off decision. Borrows `[roles] chatter`
    // - the nearest surviving single-agent-seat preference - rather than
    // falling straight to `agent::pick`'s own default order, so an operator
    // who has already named a preferred seat there is not silently
    // overridden for this decision too.
    let spec: AgentSpec = agent::pick(
        &state.config.agents,
        crate::config::primary(state.config.roles.chatter.as_ref()),
        &agent::installed,
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
            attachments: &[],
            writable: &[],
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
                progress.version = Some(
                    Version::parse(&base_version)?
                        .bump(decision.level)
                        .to_string(),
                );
                escalate_pending(state, &repo, &remote, &p, &decision, &base_version, &marker).await
            }
        };
    }

    let next = Version::parse(&base_version)?
        .bump(decision.level)
        .to_string();
    progress.version = Some(next.clone());
    let branch = format!("chore/release-v{next}");
    let worktree = state.dir().join("bump");
    let (shared, branch_ref, next_ref, decision_ref) = (&*state, &branch, &next, &decision);
    let (pr_url_opened, outcome) =
        release_attempt(
            &repo,
            &remote,
            &base,
            &worktree,
            &branch,
            |head| {
                let repo = repo.clone();
                async move { gh_open_pr_for_head(&repo, &head).await }
            },
            |wt| async move {
                open_bump_pr(shared, &wt, branch_ref, next_ref, decision_ref, pr_url).await
            },
        )
        .await?;
    let (automerge_warning, merged_detail) = match outcome {
        AutomergeOutcome::Enabled => (None, None),
        AutomergeOutcome::MergedDirectly { detail } => (None, Some(detail)),
        AutomergeOutcome::Failed { reason } => (Some(reason), None),
    };

    // The pull request exists on the forge the moment `open_bump_pr` returns
    // its URL, regardless of what happens next - so the event that names it
    // is unconditional, and a marker write failing (a full disk, a missing
    // `home/bump` directory) is reported as its own warning rather than
    // swallowing that URL entirely the way propagating it with `?` would.
    // `find_open_release_pr` is the fallback if this leaves no local record:
    // the next merge that finds no marker still finds this pull request on
    // the forge before opening a second one.
    let marker_write = write_marker(
        &marker,
        &PendingBump {
            target_version: next.clone(),
            level: decision.level,
            branch,
            pr_url: pr_url_opened.clone(),
        },
    );
    state.event(
        "bump",
        format!(
            "opened a {} release bump to v{next} ({}): {pr_url_opened}",
            decision.level.as_str(),
            decision.reason
        ),
    );
    if let Err(e) = marker_write {
        state.event(
            "bump",
            format!(
                "could not record the pending release bump marker for v{next}: {e:#}; a later \
                 merge may open a duplicate pull request if it cannot find {pr_url_opened} on \
                 the forge either"
            ),
        );
    }
    state.release_bump = Some(run::ReleaseBump {
        pr_url: Some(pr_url_opened.clone()),
        version: Some(next.clone()),
        automerge_enabled: automerge_warning.is_none() && merged_detail.is_none(),
        merged_directly: merged_detail.is_some(),
        ..run::ReleaseBump::default()
    });
    if let Some(detail) = merged_detail {
        // Merged already: a pending marker would make the next merge wait on
        // a pull request that is gone.
        clear_marker(&marker);
        state.event("bump", format!("merged v{next} directly: {detail}"));
    }
    if let Some(warning) = automerge_warning {
        state.event(
            "bump",
            format!("could not enable automerge on {pr_url_opened}: {warning}; merge it by hand"),
        );
        report_problem(state, Some(&pr_url_opened), Some(&next), &warning).await;
    }
    Ok(())
}

/// The node name post-merge notices were filed under, back when they were
/// questions. Kept because [`ask::Questions::settle_run`] must go on exempting
/// any such question still open on disk; new problems go to
/// [`crate::notices`] instead - nothing here is something to answer.
pub const NOTICE_NODE: &str = "release-bump";

/// What to tell the operator to do about a refused `gh pr merge --auto`.
///
/// Keys on the wording GitHub is known to use when the base branch has no
/// required status checks (`enablePullRequestAutoMerge` / "protected branch
/// rules"); anything else falls back to the generic advice, with the reason
/// carried verbatim next to it.
fn automerge_hint(reason: &str) -> &'static str {
    let r = reason.to_lowercase();
    if is_clean_status_refusal(reason) {
        "merge the release pull request by hand; CI is already green"
    } else if r.contains("enablepullrequestautomerge") || r.contains("protected branch rules") {
        "merge the release pull request by hand, and enable branch protection with required \
         status checks on the base branch so automerge can work next time"
    } else {
        "merge the release pull request by hand"
    }
}

/// The comment left on the release pull request itself.
fn automerge_failure_comment(reason: &str) -> String {
    format!(
        "magi could not enable automerge on this pull request: {reason}\n\n\
         Action required: {}. Until then the release does not happen.",
        automerge_hint(reason)
    )
}

/// Record a post-merge problem on the run and raise the operator-facing
/// notification, without touching the forge. Returns the notice and the
/// comment body meant for the release pull request when there is one.
///
/// Split from [`report_problem`] so the state, the notice and the wording
/// can be asserted without a `gh`. The notice is keyed on the run, so a retry
/// of the same failed bump folds into one entry instead of flooding the bell.
#[cfg(test)]
fn surface_problem(
    state: &mut RunState,
    store: &Notices,
    pr_url: Option<&str>,
    version: Option<&str>,
    reason: &str,
) -> Result<(Notice, Option<String>)> {
    surface_problem_in(state, store, pr_url, version, reason, false)
}

/// [`surface_problem`], where `pending` says `pr_url` is a release pull
/// request that already existed and could not be raised to `version` - not one
/// whose automerge failed. It is linked and recorded, but gets no comment.
fn surface_problem_in(
    state: &mut RunState,
    store: &Notices,
    pr_url: Option<&str>,
    version: Option<&str>,
    reason: &str,
    pending: bool,
) -> Result<(Notice, Option<String>)> {
    let action = if let (true, Some(url)) = (pending, pr_url) {
        let ja = crate::lang::is_japanese(&state.config.graph.language);
        let first = reason.lines().next().filter(|l| !l.is_empty());
        let cause = first.map(|l| format!(" ({l})")).unwrap_or_default();
        if ja {
            let target = version.map(|v| format!(" v{v}")).unwrap_or_default();
            format!(
                "既存のリリース PR {url} を{target}へ更新できませんでした{cause}。PR を手で更新してください"
            )
        } else {
            let target = version.map(|v| format!(" to v{v}")).unwrap_or_default();
            format!(
                "the pending release pull request {url} could not be updated{target}{cause}; update it by hand"
            )
        }
    } else if pr_url.is_some() {
        automerge_hint(reason).to_owned()
    } else {
        let ja = crate::lang::is_japanese(&state.config.graph.language);
        let first = reason.lines().next().filter(|l| !l.is_empty());
        if ja {
            let target = version.map(|v| format!(" (v{v})")).unwrap_or_default();
            let cause = first.map(|l| format!(" ({l})")).unwrap_or_default();
            format!(
                "リリースバンプ{target}は実行されませんでした{cause}。リリース PR を手で開いてください"
            )
        } else {
            let target = version.map(|v| format!(" to v{v}")).unwrap_or_default();
            let cause = first.map(|l| format!(" ({l})")).unwrap_or_default();
            format!(
                "the release bump{target} did not run{cause}; open the release pull request by hand"
            )
        }
    };
    let record = state.release_bump.get_or_insert_with(Default::default);
    record.pr_url = pr_url.map(str::to_owned).or(record.pr_url.take());
    record.version = version.map(str::to_owned).or(record.version.take());
    record.automerge_enabled = false;
    record.problem = Some(reason.to_owned());
    record.action_required = Some(action.clone());

    let mut notice = Notice::error(
        &format!("release-bump:{}", state.id),
        format!(
            "Run {} merged, but its release step failed: {action}.",
            state.id
        ),
    );
    notice = match pr_url {
        Some(url) => notice.link(Link::Url {
            url: url.to_owned(),
        }),
        None => notice.link(Link::Run {
            id: state.id.clone(),
        }),
    };
    let notice = store
        .raise(notice)
        .context("raise the release-bump notification")?;
    state.event("bump", format!("needs attention: {action}"));
    let comment = pr_url
        .filter(|_| !pending)
        .map(|_| automerge_failure_comment(reason));
    Ok((notice, comment))
}

/// Make a post-merge problem visible: record it, comment on the release pull
/// request, raise a notification and run the configured notifier. Every step
/// is best-effort - a failed comment or webhook is an event, never a reason to
/// lose the record or the run.
pub async fn report_problem(
    state: &mut RunState,
    pr_url: Option<&str>,
    version: Option<&str>,
    reason: &str,
) {
    report_problem_in(state, &Notices::open(), pr_url, version, reason, false).await;
}

/// [`report_problem`] against an explicit notice store.
async fn report_problem_in(
    state: &mut RunState,
    store: &Notices,
    pr_url: Option<&str>,
    version: Option<&str>,
    reason: &str,
    pending: bool,
) {
    match surface_problem_in(state, store, pr_url, version, reason, pending) {
        Ok((notice, comment)) => {
            if let (Some(url), Some(body)) = (pr_url, comment)
                && let Err(e) = gh_pr_comment(
                    &state.repo,
                    url,
                    &crate::scrub::scrub(&body, &crate::scrub::Identity::current()),
                )
                .await
            {
                state.event("bump", format!("could not comment on {url}: {e:#}"));
            }
            // The webhook still speaks in question terms; a transient,
            // never-stored one carries the text so it is not lost.
            let summary = match pr_url {
                Some(url) if pending => format!("Release PR could not be updated: {url}"),
                Some(url) => format!("Release PR needs a human: {url}"),
                None if crate::lang::is_japanese(&state.config.graph.language) => {
                    "リリースバンプが実行されませんでした".to_owned()
                }
                None => "Release bump did not run".to_owned(),
            };
            let q = ask::Question::new(
                state.id.clone(),
                NOTICE_NODE.to_owned(),
                "bump".to_owned(),
                summary,
                notice.message.clone(),
                Vec::new(),
            );
            if let Err(e) = ask::notify(&state.config.notify, &q).await {
                tracing::warn!(
                    "could not notify about the release bump of {}: {e:#}",
                    state.id
                );
            }
        }
        Err(e) => state.event("bump", format!("could not raise a notice: {e:#}")),
    }
}

pub(crate) async fn gh_pr_comment(cwd: &Path, pr_url: &str, body: &str) -> Result<()> {
    let out = tokio::process::Command::new("gh")
        .args(["pr", "comment", pr_url, "--body", body])
        .current_dir(cwd)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr comment")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "gh pr comment: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
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

    // Only the substantive change - the commit landing on the remote branch
    // - has to succeed for the escalation to have happened at all. Anything
    // after the push is a follow-up, not a precondition: the branch already
    // carries the new version whether or not it succeeds.
    let pushed: Result<()> = async {
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
        Ok(())
    }
    .await;
    if let Err(e) = pushed {
        git::worktree_remove(repo, &worktree).await.ok();
        return Err(e);
    }

    // The commit is on the remote branch now regardless of what happens
    // below - the title edit is cosmetic, and the marker and the event must
    // both reflect the real, already-pushed state even if it fails.
    let title_warning = match gh_pr_edit_title(
        &worktree,
        &pending.pr_url,
        &crate::scrub::scrub(
            &format!("chore: release v{next} ({} bump)", decision.level.as_str()),
            &crate::scrub::Identity::current(),
        ),
    )
    .await
    {
        Ok(()) => None,
        Err(e) => Some(e.to_string()),
    };
    git::worktree_remove(repo, &worktree).await.ok();

    let marker_write = write_marker(
        marker,
        &PendingBump {
            target_version: next.clone(),
            level: decision.level,
            branch: pending.branch.clone(),
            pr_url: pending.pr_url.clone(),
        },
    );
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
    if let Err(e) = marker_write {
        state.event(
            "bump",
            format!(
                "could not update the pending release bump marker to v{next}: {e:#}; a later \
                 merge may misjudge whether it is already covered"
            ),
        );
    }
    if let Some(warning) = title_warning {
        state.event(
            "bump",
            format!(
                "pushed v{next} to {} but could not update its title: {warning}; the squashed \
                 subject may still read the superseded version",
                pending.pr_url
            ),
        );
    }
    Ok(())
}

/// Does an open pull request have `branch` as its head? Any head repository
/// counts: the question is only whether the branch name is spoken for.
async fn gh_open_pr_for_head(repo: &Path, branch: &str) -> Result<bool> {
    let out = tokio::process::Command::new("gh")
        .args([
            "pr", "list", "--head", branch, "--state", "open", "--json", "url",
        ])
        .current_dir(repo)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .context("spawn gh pr list")?;
    if !out.status.success() {
        bail!(
            "gh pr list --head {branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let prs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("parse the pull requests headed by {branch}"))?;
    Ok(!prs.is_empty())
}

/// Create the release branch and worktree, run `fill` in it, and leave nothing
/// behind unless a pull request came out of it.
///
/// A leftover `chore/release-vX.Y.Z` from an earlier failed attempt is
/// inspected rather than fatal ([`reclaim_stale_branch`]), once per attempt. A
/// failure of `fill` - or of creating the worktree - removes the worktree and
/// the branch this attempt made, except a branch that reached the remote,
/// which is kept and said so in the returned error.
async fn release_attempt<T, F, Fut, P, PFut>(
    repo: &Path,
    remote: &str,
    base: &str,
    worktree: &Path,
    branch: &str,
    open_pr: P,
    fill: F,
) -> Result<T>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
    P: Fn(String) -> PFut,
    PFut: std::future::Future<Output = Result<bool>>,
{
    let start = format!("{remote}/{base}");
    git::worktree_remove(repo, worktree).await.ok();
    let mut retried = false;
    if git::branch_exists(repo, branch).await? {
        reclaim_stale_branch(repo, remote, &start, branch, &open_pr).await?;
        retried = true;
    }
    loop {
        // Two steps instead of `worktree add -b`, so that who made the branch
        // is known from which step failed rather than guessed from git's
        // wording or the branch's tip: `git branch` creates the ref atomically
        // or fails, and only a branch it created is this attempt's.
        if let Err(e) = git::git(repo, &["branch", branch, &start]).await {
            let e = e.context("create the release branch");
            if !git::branch_exists(repo, branch).await.unwrap_or(false) {
                return Err(e);
            }
            // It appeared since the check above: somebody else's.
            if retried {
                return Err(anyhow!(
                    "{e:#}; left {branch} in place: it is not one this attempt created"
                ));
            }
            retried = true;
            if let Err(r) = reclaim_stale_branch(repo, remote, &start, branch, &open_pr).await {
                return Err(anyhow!("{r:#} (after: {e:#})"));
            }
            continue;
        }
        if let Some(parent) = worktree.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let path = worktree.to_string_lossy();
        if let Err(e) = git::git(repo, &["worktree", "add", &path, branch]).await {
            let e = e.context("create the release-bump worktree");
            return Err(discard_attempt(repo, remote, worktree, branch, e).await);
        }
        break;
    }
    match fill(worktree.to_path_buf()).await {
        Ok(v) => {
            // The branch lives on in the pull request; the worktree is
            // throwaway and a stale one would collide with the next attempt.
            git::worktree_remove(repo, worktree).await.ok();
            Ok(v)
        }
        Err(e) => Err(discard_attempt(repo, remote, worktree, branch, e).await),
    }
}

/// Undo what a failed attempt created and fold the outcome into its error.
async fn discard_attempt(
    repo: &Path,
    remote: &str,
    worktree: &Path,
    branch: &str,
    cause: anyhow::Error,
) -> anyhow::Error {
    git::worktree_remove(repo, worktree).await.ok();
    if !git::branch_exists(repo, branch).await.unwrap_or(false) {
        return cause;
    }
    // The remote's own answer decides, not whether a push was attempted: a
    // push that failed half way may still have landed the ref.
    let note = match git::remote_has_branch(repo, remote, branch).await {
        Ok(false) => match git::branch_delete(repo, branch).await {
            Ok(true) => return cause,
            _ => format!("could not delete the local branch {branch}"),
        },
        Ok(true) => format!("left {branch} in place: it was pushed to {remote}"),
        Err(e) => format!(
            "left {branch} in place: could not tell whether it was pushed to {remote} ({e:#})"
        ),
    };
    anyhow!("{cause:#}; {note}")
}

/// A branch of the release's name already exists. Delete it so the attempt can
/// go on if and only if it holds nothing: not on the remote, no open pull
/// request, and no commit that `start` (`<remote>/<base>`) lacks. Anything
/// unproven fails closed with a reason that names the branch.
async fn reclaim_stale_branch<P, PFut>(
    repo: &Path,
    remote: &str,
    start: &str,
    branch: &str,
    open_pr: &P,
) -> Result<()>
where
    P: Fn(String) -> PFut,
    PFut: std::future::Future<Output = Result<bool>>,
{
    match git::remote_has_branch(repo, remote, branch).await {
        Ok(false) => {}
        Ok(true) => bail!("the branch {branch} already exists and is on {remote}; left alone"),
        Err(e) => bail!(
            "the branch {branch} already exists and could not check whether {remote} has it              ({e:#}); left alone"
        ),
    }
    match open_pr(branch.to_string()).await {
        Ok(false) => {}
        Ok(true) => {
            bail!("the branch {branch} already exists and has an open pull request; left alone")
        }
        Err(e) => bail!(
            "the branch {branch} already exists and could not check for an open pull request \
             ({e:#}); left alone"
        ),
    }
    match git::commits_ahead(repo, start, branch).await {
        Ok(0) => {}
        Ok(n) => bail!(
            "the branch {branch} already exists with {n} commit(s) not in {start}; left alone"
        ),
        Err(e) => bail!(
            "the branch {branch} already exists and could not compare it with {start} ({e:#}); \
             left alone"
        ),
    }
    if let Some(held) = git::worktree_holding(repo, branch).await? {
        let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => a == b,
        };
        if same(&held, repo) {
            bail!(
                "the branch {branch} already exists and is checked out in the main checkout; left alone"
            );
        }
        git::worktree_remove(repo, &held).await.ok();
    }
    if !git::branch_delete(repo, branch).await? {
        bail!("the branch {branch} already exists and could not be deleted; left alone");
    }
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
) -> Result<(String, AutomergeOutcome)> {
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

    let (title, body) = release_pr(
        decision.level.as_str(),
        &decision.reason,
        next_version,
        &state.id,
        source_pr_url,
    );
    let who = crate::scrub::Identity::current();
    let (title, body) = (
        crate::scrub::scrub(&title, &who),
        crate::scrub::scrub(&body, &who),
    );
    let url = gh_pr_create(worktree, &state.base_branch, branch, &title, &body).await?;
    let outcome = match gh_enable_automerge(worktree, &url).await {
        Ok(()) => AutomergeOutcome::Enabled,
        Err(e) => {
            let reason = e.to_string();
            if is_clean_status_refusal(&reason) {
                gh_merge_directly(worktree, &url, &title, reason).await
            } else {
                AutomergeOutcome::Failed { reason }
            }
        }
    };
    Ok((url, outcome))
}

/// What became of the release pull request's path to `main`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutomergeOutcome {
    /// Automerge is armed; CI green will merge it.
    Enabled,
    /// CI beat us to it, so magi merged the pull request itself.
    MergedDirectly { detail: String },
    /// Neither worked; a human has to merge it.
    Failed { reason: String },
}

/// Is this GitHub's refusal to arm automerge on a pull request that is already
/// mergeable? Automerge can only be enabled while something is still pending,
/// so a fast CI that finishes first gets `Pull request is in clean status`.
/// Both fragments are required so an unrelated "clean status" wording or a
/// branch-protection refusal does not trigger a direct merge.
fn is_clean_status_refusal(reason: &str) -> bool {
    let r = reason.to_lowercase();
    r.contains("is in clean status") && r.contains("enablepullrequestautomerge")
}

/// The direct merge, in the same shape [`land::merge_argv`] uses but addressed
/// by URL: squash under the pull request's own title, delete the branch.
fn bump_merge_argv(pr_url: &str, subject: &str) -> Vec<String> {
    [
        "pr",
        "merge",
        pr_url,
        "--squash",
        "--delete-branch",
        "--subject",
        subject,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// Decide what a direct merge amounted to. No I/O. A zero exit is a merge; a
/// non-zero one is judged by the forge (`after`), never by the exit code, and
/// an unreadable forge is not evidence of success.
fn resolve_direct_merge(
    refusal: &str,
    argv: &[String],
    merge_ok: bool,
    stderr: &str,
    after: Option<land::PrLifecycle>,
) -> AutomergeOutcome {
    if merge_ok {
        return AutomergeOutcome::MergedDirectly {
            detail: format!("automerge was refused ({refusal}); gh {}", argv.join(" ")),
        };
    }
    match land::merged_after_all(argv, stderr, after) {
        Some(m) => AutomergeOutcome::MergedDirectly { detail: m.detail },
        None => AutomergeOutcome::Failed {
            reason: format!("{refusal}; merging directly failed too: {}", stderr.trim()),
        },
    }
}

/// Merge the pull request directly after automerge was refused as clean.
/// Every failure lands in [`AutomergeOutcome::Failed`] so the caller keeps the
/// warning-and-comment path.
async fn gh_merge_directly(
    cwd: &Path,
    pr_url: &str,
    subject: &str,
    refusal: String,
) -> AutomergeOutcome {
    let argv = bump_merge_argv(pr_url, subject);
    let out = match tokio::process::Command::new("gh")
        .args(&argv)
        .current_dir(cwd)
        .quiet()
        .stdin(std::process::Stdio::null())
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            return AutomergeOutcome::Failed {
                reason: format!("{refusal}; could not spawn gh to merge directly: {e}"),
            };
        }
    };
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    // jj keeps HEAD detached, so `--delete-branch` exits non-zero after the
    // merge has happened; ask the forge.
    let after = if out.status.success() {
        None
    } else {
        land::lifecycle(cwd, pr_url).await.ok()
    };
    resolve_direct_merge(&refusal, &argv, out.status.success(), &stderr, after)
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

/// Title and body of a release bump pull request. Fixed English whatever
/// `[graph] language` says: it lands on GitHub. Pure so a test can hold it to
/// that. (`reason` comes from the decision seat, which the prompt tells to
/// write English.)
fn release_pr(
    level: &str,
    reason: &str,
    next_version: &str,
    run_id: &str,
    source_pr_url: &str,
) -> (String, String) {
    let title = format!("chore: release v{next_version} ({level} bump)");
    let body = format!(
        "## Background\n\n\
         A change that was just merged is a `{level}` change, so the crate needs a new \
         release: {reason}\n\n\
         Triggered by magi run `{run_id}`, which landed {source}.\n\n\
         ## Change\n\n\
         Raises the package version to `v{next_version}` in `Cargo.toml`, with \
         `Cargo.lock` following it. Nothing else changes.\n\n\
         ## Risk\n\n\
         Version-bump-only, so there is nothing here for a reviewer to find. Merging \
         it starts the release pipeline (auto-tag, then the release workflow).",
        source = source_pr_url,
    );
    (title, body)
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
    use crate::notices::Severity;

    #[test]
    fn github_facing_bump_text_is_english() {
        let (title, body) =
            release_pr("minor", "adds a flag", "0.37.0", "ab12", "https://x/pull/1");
        assert!(title.is_ascii() && body.is_ascii(), "{title}\n{body}");
        assert_eq!(title, "chore: release v0.37.0 (minor bump)");
        assert!(
            body.contains("## Background") && body.contains("## Change"),
            "{body}"
        );
        let p = decision_prompt("s", "i", "d", &[], "0.36.5");
        assert!(p.contains(crate::prompt::GITHUB_ENGLISH_HEADING), "{p}");
    }
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

    /// A bare `origin` plus a clone of it, `main` pushed with `files`.
    async fn origin_with(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin.git");
        let repo = dir.path().join("repo");
        let o = origin.to_string_lossy().into_owned();
        git::git(dir.path(), &["init", "--bare", "-b", "main", &o])
            .await
            .unwrap();
        tokio::fs::create_dir_all(&repo).await.unwrap();
        git::git(&repo, &["init", "-b", "main"]).await.unwrap();
        git::git(&repo, &["config", "user.name", "test"])
            .await
            .unwrap();
        git::git(&repo, &["config", "user.email", "test@example.com"])
            .await
            .unwrap();
        for (name, body) in files {
            tokio::fs::write(repo.join(name), body).await.unwrap();
        }
        git::git(&repo, &["add", "-A"]).await.unwrap();
        git::git(&repo, &["commit", "-m", "init"]).await.unwrap();
        git::git(&repo, &["remote", "add", "origin", &o])
            .await
            .unwrap();
        git::git(&repo, &["push", "origin", "main"]).await.unwrap();
        (dir, repo)
    }

    async fn no_pr(_: String) -> Result<bool> {
        Ok(false)
    }

    /// `origin_with` plus a branch `b` made from `main`, optionally one commit
    /// ahead of it.
    async fn with_branch(ahead: bool) -> (tempfile::TempDir, PathBuf) {
        let (d, repo) = origin_with(&[("f", "x\n")]).await;
        git::git(&repo, &["branch", "b"]).await.unwrap();
        if ahead {
            git::git(&repo, &["checkout", "-q", "b"]).await.unwrap();
            git::git(&repo, &["commit", "--allow-empty", "-m", "wip"])
                .await
                .unwrap();
            git::git(&repo, &["checkout", "-q", "main"]).await.unwrap();
        }
        (d, repo)
    }

    async fn attempt(repo: &Path, open_pr: bool, fail_after_push: Option<bool>) -> Result<()> {
        let wt = repo.parent().unwrap().join("bump");
        release_attempt(
            repo,
            "origin",
            "main",
            &wt,
            "b",
            |_| async move { Ok(open_pr) },
            |w| async move {
                if fail_after_push == Some(true) {
                    git::push(&w, "origin", "b").await?;
                }
                if fail_after_push.is_some() {
                    bail!("cargo build failed");
                }
                Ok(())
            },
        )
        .await
    }

    #[tokio::test]
    async fn a_stale_ancestor_branch_is_deleted_and_the_attempt_proceeds() {
        let (_d, repo) = with_branch(false).await;
        attempt(&repo, false, None).await.unwrap();
        // Success keeps the branch (it backs the pull request).
        assert!(git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn a_stale_branch_holding_a_leftover_worktree_is_reclaimed() {
        let (_d, repo) = with_branch(false).await;
        let old = repo.parent().unwrap().join("old");
        git::git(&repo, &["worktree", "add", &old.to_string_lossy(), "b"])
            .await
            .unwrap();
        attempt(&repo, false, None).await.unwrap();
        assert!(!old.exists());
    }

    #[tokio::test]
    async fn a_stale_branch_with_an_unmerged_commit_is_kept_with_a_reason() {
        let (_d, repo) = with_branch(true).await;
        let e = attempt(&repo, false, None).await.unwrap_err().to_string();
        assert!(e.contains("`b`") || e.contains("branch b"), "{e}");
        assert!(e.contains("1 commit(s) not in origin/main"), "{e}");
        assert!(git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn a_stale_branch_on_the_remote_is_kept() {
        let (_d, repo) = with_branch(false).await;
        git::git(&repo, &["push", "origin", "b"]).await.unwrap();
        let e = attempt(&repo, false, None).await.unwrap_err().to_string();
        assert!(e.contains("branch b") && e.contains("is on origin"), "{e}");
        assert!(git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn a_stale_branch_with_an_open_pull_request_is_kept() {
        let (_d, repo) = with_branch(false).await;
        let e = attempt(&repo, true, None).await.unwrap_err().to_string();
        assert!(e.contains("open pull request"), "{e}");
        assert!(git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn an_unanswerable_pull_request_check_keeps_the_branch() {
        let (_d, repo) = with_branch(false).await;
        let wt = repo.parent().unwrap().join("bump");
        let e = release_attempt(
            &repo,
            "origin",
            "main",
            &wt,
            "b",
            |_| async { bail!("offline") },
            |_| async { Ok(()) },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("could not check for an open pull request"),
            "{e}"
        );
        assert!(git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn a_failed_attempt_removes_its_own_worktree_and_branch() {
        let (_d, repo) = origin_with(&[("f", "x\n")]).await;
        let wt = repo.parent().unwrap().join("bump");
        let e = attempt(&repo, false, Some(false)).await.unwrap_err();
        assert!(format!("{e:#}").contains("cargo build failed"));
        assert!(!wt.exists());
        assert!(!git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn a_pushed_branch_survives_a_failed_attempt_and_the_reason_says_so() {
        let (_d, repo) = origin_with(&[("f", "x\n")]).await;
        let e = attempt(&repo, false, Some(true)).await.unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("cargo build failed") && e.contains("pushed to origin"),
            "{e}"
        );
        assert!(git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn a_branch_left_by_a_half_done_worktree_add_is_removed() {
        // No pre-existing branch; the worktree path sits under a regular
        // file, so `worktree add -b` may create the branch and then fail.
        let (_d, repo) = origin_with(&[("f", "x\n")]).await;
        let blocker = repo.parent().unwrap().join("blocker");
        tokio::fs::write(&blocker, "file").await.unwrap();
        let wt = blocker.join("bump");
        let r = release_attempt(
            &repo,
            "origin",
            "main",
            &wt,
            "b",
            |_| async { bail!("offline") },
            |_| async { Ok(()) },
        )
        .await;
        assert!(r.is_err());
        assert!(!git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn the_retry_is_taken_at_most_once() {
        // The branch is reclaimed, but the worktree path cannot be created, so
        // the second creation fails too: no third try, and no loop.
        let (_d, repo) = with_branch(false).await;
        let blocker = repo.parent().unwrap().join("blocker");
        tokio::fs::write(&blocker, "file").await.unwrap();
        let wt = blocker.join("bump");
        let r = release_attempt(&repo, "origin", "main", &wt, "b", no_pr, |_| async {
            Ok(())
        })
        .await;
        assert!(r.is_err());
        assert!(!git::branch_exists(&repo, "b").await.unwrap());
    }

    #[tokio::test]
    async fn base_has_cargo_toml_tells_rust_from_non_rust() {
        let (_d, rust) = origin_with(&[("Cargo.toml", "[package]\nversion = \"0.1.0\"\n")]).await;
        assert!(base_has_cargo_toml(&rust, "origin", "main").await.unwrap());
        let (_d2, other) = origin_with(&[("README.md", "hi\n")]).await;
        assert!(!base_has_cargo_toml(&other, "origin", "main").await.unwrap());
        // An unresolvable ref is a fault, not "not Rust".
        assert!(base_has_cargo_toml(&other, "origin", "nope").await.is_err());
    }

    #[tokio::test]
    async fn a_repo_without_cargo_toml_skips_with_one_event_and_no_lock() {
        let (_d, repo) = origin_with(&[("README.md", "hi\n")]).await;
        let home = tempfile::tempdir().unwrap();
        let mut state = RunState::new(
            repo.clone(),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        state.candidates.push(crate::run::Candidate {
            index: 0,
            label: 'A',
            agent: "x".to_owned(),
            branch: "main".to_owned(),
            worktree: repo.clone(),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        });
        state.tally = Some(
            serde_json::from_value(serde_json::json!({
                "first_choice": {}, "borda": {}, "winner": "A",
                "unanimous_initial": true, "deliberated": false,
                "changed_votes": 0, "unanimous_final": true,
            }))
            .unwrap(),
        );
        after_merge_at(
            &mut state,
            "https://example.invalid/pull/1",
            Some(home.path()),
            None,
        )
        .await
        .expect("a non-Rust repository is not an error");
        let bumps: Vec<_> = state.events.iter().filter(|e| e.node == "bump").collect();
        assert_eq!(bumps.len(), 1, "{:?}", state.events);
        assert_eq!(
            bumps[0].message,
            "release bump: no Cargo.toml on the base branch; release bumps are Rust-only, skipping"
        );
        assert!(
            std::fs::read_dir(home.path()).unwrap().next().is_none(),
            "no marker and no lock may be created"
        );
    }

    fn winner_state(repo: &Path, base: &str) -> RunState {
        let mut state = RunState::new(
            repo.to_path_buf(),
            base.to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        state.candidates.push(crate::run::Candidate {
            index: 0,
            label: 'A',
            agent: "x".to_owned(),
            branch: "main".to_owned(),
            worktree: repo.to_path_buf(),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            verified_noop: None,
            duration_ms: 0,
            folded: false,
        });
        state.tally = Some(
            serde_json::from_value(serde_json::json!({
                "first_choice": {}, "borda": {}, "winner": "A",
                "unanimous_initial": true, "deliberated": false,
                "changed_votes": 0, "unanimous_final": true,
            }))
            .unwrap(),
        );
        state
    }

    #[tokio::test]
    async fn a_real_failure_without_a_pr_raises_one_notice_and_a_retry_folds_into_it() {
        let (_d, repo) =
            origin_with(&[("Cargo.toml", "[package]\nname=\"x\"\nversion=\"0.1.0\"\n")]).await;
        let home = tempfile::tempdir().unwrap();
        let store = Notices::at(home.path().join("notifications"));
        // An unresolvable base is a fault, not "not eligible".
        let mut state = winner_state(&repo, "nope");
        let url = "https://example.invalid/pull/1";
        after_merge_at(&mut state, url, Some(home.path()), Some(&store))
            .await
            .expect_err("an unresolvable base is a failure");
        let listed = store.list();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert!(
            listed[0].message.contains("did not run"),
            "{}",
            listed[0].message
        );
        assert!(
            listed[0].message.contains("Cargo.toml"),
            "{}",
            listed[0].message
        );
        assert!(state.events.iter().any(|e| e.node == "bump"));
        after_merge_at(&mut state, url, Some(home.path()), Some(&store))
            .await
            .expect_err("still failing");
        let listed = store.list();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].count, 2);
    }

    #[tokio::test]
    async fn not_eligible_cases_raise_no_notice() {
        let home = tempfile::tempdir().unwrap();
        let store = Notices::at(home.path().join("notifications"));
        let url = "https://example.invalid/pull/1";
        // No Cargo.toml on the base branch.
        let (_d, repo) = origin_with(&[("README.md", "hi\n")]).await;
        let mut state = winner_state(&repo, "main");
        after_merge_at(&mut state, url, Some(home.path()), Some(&store))
            .await
            .unwrap();
        // Disabled.
        let mut state = winner_state(&repo, "nope");
        state.config.merge.release_bump = false;
        after_merge_at(&mut state, url, Some(home.path()), Some(&store))
            .await
            .unwrap();
        // No winner.
        let mut state = winner_state(&repo, "nope");
        state.tally = None;
        after_merge_at(&mut state, url, Some(home.path()), Some(&store))
            .await
            .unwrap();
        assert!(store.list().is_empty(), "{:?}", store.list());
    }

    #[test]
    fn the_no_pr_notice_follows_the_configured_language() {
        let dir = tempfile::tempdir().unwrap();
        let store = Notices::at(dir.path().join("notifications"));
        let mut state = merged_state();
        state.config.graph.language = "ja".to_owned();
        let (n, _) = surface_problem(&mut state, &store, None, Some("0.2.0"), "boom").unwrap();
        assert!(n.message.contains("実行されませんでした"), "{}", n.message);
        assert!(n.message.contains("boom") && n.message.contains("v0.2.0"));
    }

    #[test]
    fn a_failed_escalation_points_at_the_pending_pr_without_commenting() {
        let dir = tempfile::tempdir().unwrap();
        let store = Notices::at(dir.path().join("notifications"));
        let mut state = merged_state();
        let url = "https://example.invalid/pull/9";
        let (n, comment) = surface_problem_in(
            &mut state,
            &store,
            Some(url),
            Some("0.3.0"),
            "push failed",
            true,
        )
        .unwrap();
        assert!(comment.is_none());
        assert!(
            n.message.contains(url) && n.message.contains("v0.3.0"),
            "{}",
            n.message
        );
        assert!(!n.message.contains("open the release pull request by hand"));
        assert!(matches!(n.link, Some(Link::Url { .. })));
    }

    #[test]
    fn a_failure_notice_names_the_pending_pr_and_the_first_line_of_the_cause() {
        let err = anyhow!("outer context").context("cargo build failed\nsecond line");
        let with_pr = Progress {
            version: Some("0.2.0".into()),
            pending_pr: Some("https://example.invalid/pull/9".into()),
        };
        let (v, reason) = notice_for_failure(&with_pr, &err);
        assert_eq!(v.as_deref(), Some("0.2.0"));
        assert_eq!(reason, "cargo build failed");
        let (v, reason) = notice_for_failure(&Progress::default(), &err);
        assert_eq!(v, None);
        assert_eq!(reason, "cargo build failed");
    }

    const NO_RULES: &str = "gh pr merge --auto: GraphQL: Pull request Branch does not have \
                            required protected branch rules (enablePullRequestAutoMerge)";

    fn merged_state() -> RunState {
        // `report::run` prints `state.dir()`; pin the global home like the
        // report tests do so nothing reaches the operator's real one.
        run::pin_test_home();
        let mut s = RunState::new(
            PathBuf::from("/no/such/repo"),
            "main".to_owned(),
            "0000000000000000000000000000000000000000".to_owned(),
            "task".to_owned(),
            Config::default(),
        );
        s.status = RunStatus::Merged;
        s
    }

    #[test]
    fn the_known_automerge_refusal_names_branch_protection() {
        assert!(automerge_hint(NO_RULES).contains("branch protection with required"));
        let other = automerge_hint("gh: network unreachable");
        assert!(!other.contains("branch protection"), "{other}");
        let body = automerge_failure_comment(NO_RULES);
        assert!(body.contains("enablePullRequestAutoMerge"), "{body}");
        assert!(body.contains("Action required"), "{body}");
    }

    const CLEAN: &str = "gh pr merge --auto: GraphQL: Pull request Pull request is in clean \
                         status (enablePullRequestAutoMerge)";

    #[test]
    fn clean_status_refusal_is_matched_narrowly() {
        assert!(is_clean_status_refusal(CLEAN));
        assert!(!is_clean_status_refusal(NO_RULES));
        assert!(!is_clean_status_refusal("gh: network unreachable"));
        assert!(!is_clean_status_refusal("Pull request is in clean status"));
        assert!(automerge_hint(CLEAN).contains("already green"));
    }

    #[test]
    fn the_direct_merge_argv_matches_the_land_flags() {
        let a = bump_merge_argv("https://github.com/o/r/pull/9", "chore: release v1.0.0");
        let l = land::merge_argv(9, "chore: release v1.0.0");
        assert_eq!(a[..2], l[..2]);
        assert_eq!(a[3..], l[3..]);
        assert_eq!(a[2], "https://github.com/o/r/pull/9");
    }

    #[test]
    fn a_direct_merge_is_judged_by_the_forge_not_the_exit_code() {
        let argv = bump_merge_argv("u", "t");
        let merged = |o: &AutomergeOutcome| matches!(o, AutomergeOutcome::MergedDirectly { .. });
        assert!(merged(&resolve_direct_merge(CLEAN, &argv, true, "", None)));
        let detached = "not on any branch";
        assert!(merged(&resolve_direct_merge(
            CLEAN,
            &argv,
            false,
            detached,
            Some(PrLifecycle::Merged)
        )));
        for after in [Some(PrLifecycle::Open), None] {
            let o = resolve_direct_merge(CLEAN, &argv, false, "boom", after);
            match o {
                AutomergeOutcome::Failed { reason } => {
                    assert!(
                        reason.contains("clean status") && reason.contains("boom"),
                        "{reason}"
                    )
                }
                other => panic!("expected Failed, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_directly_merged_bump_is_not_reported_as_pending_or_failed() {
        let mut state = merged_state();
        state.release_bump = Some(run::ReleaseBump {
            pr_url: Some("https://github.com/o/r/pull/9".to_owned()),
            version: Some("1.0.0".to_owned()),
            merged_directly: true,
            ..run::ReleaseBump::default()
        });
        assert!(!state.needs_attention());
        let text = crate::report::run(&state);
        assert!(text.contains("merged directly"), "{text}");
        assert!(!text.contains("FAILED"), "{text}");
    }

    #[test]
    fn an_automerge_failure_is_recorded_shown_and_filed_and_survives_settling() {
        let dir = tempfile::tempdir().unwrap();
        let store = Notices::at(dir.path().join("notifications"));
        let questions = ask::Questions::at(dir.path().join("questions"));
        let mut state = merged_state();
        let url = "https://github.com/o/r/pull/35";

        let (n, comment) =
            surface_problem(&mut state, &store, Some(url), Some("0.8.0"), NO_RULES).unwrap();

        // The comment goes on the release PR and carries reason and fix.
        let comment = comment.expect("a PR was opened, so it gets a comment");
        assert!(comment.contains("branch protection"), "{comment}");

        // The run is still Merged, but no longer reads as plain green.
        assert_eq!(state.status, RunStatus::Merged);
        assert!(state.needs_attention());
        let text = crate::report::run(&state);
        assert!(text.contains("release bump"), "{text}");
        assert!(text.contains("FAILED"), "{text}");
        assert!(text.contains(url), "{text}");
        assert!(text.contains("action required"), "{text}");
        assert!(crate::report::line(&state).contains("release needs a human"));

        // Exactly one notification, linked to the PR, and no question.
        assert_eq!(n.severity, Severity::Error);
        assert_eq!(
            n.link,
            Some(Link::Url {
                url: url.to_owned()
            })
        );
        assert_eq!(store.list().len(), 1);
        assert!(questions.open_for(&state.id).is_empty());

        // A retry of the same failure folds into the same entry.
        surface_problem(&mut state, &store, Some(url), Some("0.8.0"), NO_RULES).unwrap();
        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].count, 2);
    }

    #[test]
    fn a_bump_that_never_ran_is_surfaced_without_a_pr_comment() {
        let dir = tempfile::tempdir().unwrap();
        let store = Notices::at(dir.path().join("notifications"));
        let mut state = merged_state();
        let (n, comment) = surface_problem(&mut state, &store, None, None, "no agent").unwrap();
        assert!(comment.is_none());
        assert!(matches!(n.link, Some(Link::Run { .. })));
        assert!(state.needs_attention());
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

    /// The shape `kanadehq/kanade` has: a workspace root with member crates
    /// but no crate of its own, so `[package]` never exists and the version
    /// lives under `[workspace.package]` alone. Before this fell back,
    /// `rewrite_cargo_version` bailed on every such repository and no bump
    /// pull request was ever opened for it.
    #[test]
    fn cargo_version_rewrite_falls_back_to_workspace_package_without_a_package_table() {
        let toml = "\
[workspace]\n\
members = [\"crates/a\", \"crates/b\"]\n\
\n\
[workspace.package]\n\
version = \"0.45.18\"\n\
edition = \"2024\"\n\
\n\
[workspace.dependencies]\n\
foo = { version = \"1.2.3\" }\n";
        let out = rewrite_cargo_version(toml, "0.45.19").unwrap();
        assert!(out.contains("version = \"0.45.19\""));
        assert!(
            out.contains("foo = { version = \"1.2.3\" }"),
            "a workspace dependency's own version pin must survive: {out}"
        );
        assert_eq!(
            out.lines().count(),
            toml.lines().count(),
            "the rewrite replaces one line, it does not add or remove any"
        );
    }

    /// The exact failure that happened on `kanadehq/kanade`: an internal
    /// member referenced by both `path` and `version` must have its pin
    /// bumped alongside `[workspace.package]`, in the same edit.
    #[test]
    fn cargo_version_rewrite_bumps_an_internal_workspace_dependency_pin() {
        let toml = "\
[workspace]\n\
members = [\"crates/kanade-shared\"]\n\
\n\
[workspace.package]\n\
version = \"0.48.2\"\n\
\n\
[workspace.dependencies]\n\
kanade-shared = { path = \"crates/kanade-shared\", version = \"0.48.2\" }\n";
        let out = rewrite_cargo_version(toml, "0.48.3").unwrap();
        assert!(out.contains("[workspace.package]\nversion = \"0.48.3\"\n"));
        assert!(
            out.contains(
                "kanade-shared = { path = \"crates/kanade-shared\", version = \"0.48.3\" }"
            ),
            "the internal pin must move with the workspace version: {out}"
        );
    }

    /// Key order inside the inline table must not matter.
    #[test]
    fn cargo_version_rewrite_bumps_an_internal_pin_with_version_before_path() {
        let toml = "\
[workspace.package]\n\
version = \"1.0.0\"\n\
\n\
[workspace.dependencies]\n\
inner = { version = \"1.0.0\", path = \"crates/inner\" }\n";
        let out = rewrite_cargo_version(toml, "1.0.1").unwrap();
        assert!(out.contains("inner = { version = \"1.0.1\", path = \"crates/inner\" }"));
    }

    /// The dotted-table form (`[workspace.dependencies.name]`) must be
    /// rewritten too, not only the inline-table form.
    #[test]
    fn cargo_version_rewrite_bumps_an_internal_pin_in_dotted_table_form() {
        let toml = "\
[workspace.package]\n\
version = \"2.3.0\"\n\
\n\
[workspace.dependencies.inner]\n\
path = \"crates/inner\"\n\
version = \"2.3.0\"\n";
        let out = rewrite_cargo_version(toml, "2.4.0").unwrap();
        assert!(out.contains("[workspace.package]\nversion = \"2.4.0\"\n"));
        assert!(out.contains(
            "[workspace.dependencies.inner]\npath = \"crates/inner\"\nversion = \"2.4.0\"\n"
        ));
    }

    /// The dotted-key form written as two separate lines directly under the
    /// flat `[workspace.dependencies]` table - no inline table, no
    /// `[workspace.dependencies.name]` sub-header - is also valid TOML and
    /// must have its `version` line rewritten.
    #[test]
    fn cargo_version_rewrite_bumps_an_internal_pin_in_dotted_key_form() {
        let toml = "\
[workspace.package]\n\
version = \"2.3.0\"\n\
\n\
[workspace.dependencies]\n\
inner.path = \"crates/inner\"\n\
inner.version = \"2.3.0\"\n";
        let out = rewrite_cargo_version(toml, "2.4.0").unwrap();
        assert!(out.contains("[workspace.package]\nversion = \"2.4.0\"\n"));
        assert!(out.contains("inner.path = \"crates/inner\"\ninner.version = \"2.4.0\"\n"));
    }

    /// A workspace-only member with `path` but no `version` is unpublished
    /// and must be left exactly alone.
    #[test]
    fn cargo_version_rewrite_leaves_a_path_only_workspace_dependency_untouched() {
        let toml = "\
[workspace.package]\n\
version = \"0.1.0\"\n\
\n\
[workspace.dependencies]\n\
internal-only = { path = \"crates/internal-only\" }\n";
        let out = rewrite_cargo_version(toml, "0.2.0").unwrap();
        assert!(out.contains("internal-only = { path = \"crates/internal-only\" }"));
    }

    /// An external crates.io dependency, `version` but no `path`, must never
    /// be touched by the internal-pin logic even when it sits in
    /// `[workspace.dependencies]` alongside a real internal pin.
    #[test]
    fn cargo_version_rewrite_leaves_an_external_dependency_untouched() {
        let toml = "\
[workspace.package]\n\
version = \"0.1.0\"\n\
\n\
[workspace.dependencies]\n\
serde = { version = \"1\", features = [\"derive\"] }\n\
inner = { path = \"crates/inner\", version = \"0.1.0\" }\n";
        let out = rewrite_cargo_version(toml, "0.2.0").unwrap();
        assert!(out.contains("serde = { version = \"1\", features = [\"derive\"] }"));
        assert!(out.contains("inner = { path = \"crates/inner\", version = \"0.2.0\" }"));
    }

    /// A manifest with no `[workspace.dependencies]` table at all must keep
    /// bumping `[workspace.package]` exactly as before - regression guard
    /// for the pre-existing behaviour this change extends.
    #[test]
    fn cargo_version_rewrite_without_workspace_dependencies_table_still_bumps_package() {
        let toml = "[workspace.package]\nversion = \"0.9.0\"\nedition = \"2024\"\n";
        let out = rewrite_cargo_version(toml, "0.10.0").unwrap();
        assert_eq!(
            out,
            "[workspace.package]\nversion = \"0.10.0\"\nedition = \"2024\"\n"
        );
    }

    /// The `kanadehq/kanade` shape itself: several internal-looking and
    /// external entries mixed in one `[workspace.dependencies]` table.
    #[test]
    fn cargo_version_rewrite_handles_a_kanade_shaped_workspace_dependencies_table() {
        let toml = "\
[workspace.package]\n\
version = \"0.48.2\"\n\
\n\
[workspace.dependencies]\n\
anyhow = { version = \"1\" }\n\
serde = { version = \"1\", features = [\"derive\"] }\n\
kanade-shared = { path = \"crates/kanade-shared\", version = \"0.48.2\" }\n\
kanade-core = { path = \"crates/kanade-core\", version = \"0.48.2\" }\n\
kanade-internal-tool = { path = \"crates/kanade-internal-tool\" }\n";
        let out = rewrite_cargo_version(toml, "0.48.3").unwrap();
        assert!(out.contains("anyhow = { version = \"1\" }"));
        assert!(out.contains("serde = { version = \"1\", features = [\"derive\"] }"));
        assert!(
            out.contains(
                "kanade-shared = { path = \"crates/kanade-shared\", version = \"0.48.3\" }"
            )
        );
        assert!(
            out.contains("kanade-core = { path = \"crates/kanade-core\", version = \"0.48.3\" }")
        );
        assert!(out.contains("kanade-internal-tool = { path = \"crates/kanade-internal-tool\" }"));
    }

    /// A shape the line-based rewrite cannot express - an inline table split
    /// across lines - must fail the whole bump rather than silently leave
    /// the pin stale.
    #[test]
    fn cargo_version_rewrite_bails_on_an_unrepresentable_inline_table() {
        let toml = "\
[workspace.package]\n\
version = \"0.1.0\"\n\
\n\
[workspace.dependencies]\n\
inner = { path = \"crates/inner\",\n\
    version = \"0.1.0\" }\n";
        assert!(rewrite_cargo_version(toml, "0.2.0").is_err());
    }

    /// A trailing comment on the `[workspace.dependencies]` header itself is
    /// valid TOML and must not stop the section from being recognised.
    #[test]
    fn cargo_version_rewrite_recognises_a_commented_workspace_dependencies_header() {
        let toml = "\
[workspace.package]\n\
version = \"0.1.0\"\n\
\n\
[workspace.dependencies] # internal pins\n\
inner = { path = \"crates/inner\", version = \"0.1.0\" }\n";
        let out = rewrite_cargo_version(toml, "0.2.0").unwrap();
        assert!(out.contains("inner = { path = \"crates/inner\", version = \"0.2.0\" }"));
    }

    /// A quoted dependency key (`"inner" = { .. }`) is valid TOML and must
    /// compare equal to the plain name the `toml` crate itself reports.
    #[test]
    fn cargo_version_rewrite_bumps_a_quoted_dependency_key() {
        let toml = "\
[workspace.package]\n\
version = \"0.1.0\"\n\
\n\
[workspace.dependencies]\n\
\"inner\" = { path = \"crates/inner\", version = \"0.1.0\" }\n";
        let out = rewrite_cargo_version(toml, "0.2.0").unwrap();
        assert!(out.contains("\"inner\" = { path = \"crates/inner\", version = \"0.2.0\" }"));
    }

    /// A literal (single-quoted) TOML string is a legal way to spell a
    /// version pin, and the rewrite must preserve that quoting style rather
    /// than failing to find it or silently switching it to a basic string.
    #[test]
    fn cargo_version_rewrite_bumps_a_literal_string_version_pin() {
        let toml = "\
[workspace.package]\n\
version = \"0.1.0\"\n\
\n\
[workspace.dependencies]\n\
inner = { path = 'crates/inner', version = '0.1.0' }\n";
        let out = rewrite_cargo_version(toml, "0.2.0").unwrap();
        assert!(out.contains("inner = { path = 'crates/inner', version = '0.2.0' }"));
    }

    /// A manifest whose unrelated sections would not satisfy a strict
    /// whole-file TOML parse (here: a duplicate top-level table, which the
    /// `toml` crate rejects) must still have its `[workspace.dependencies]`
    /// pins rewritten - only that section's own syntax is a precondition,
    /// not the rest of the file.
    #[test]
    fn cargo_version_rewrite_does_not_require_the_whole_file_to_parse() {
        let toml = "\
[workspace.package]\n\
version = \"0.1.0\"\n\
\n\
[workspace.dependencies]\n\
inner = { path = \"crates/inner\", version = \"0.1.0\" }\n\
\n\
[workspace.package]\n\
edition = \"2024\"\n";
        let out = rewrite_cargo_version(toml, "0.2.0").unwrap();
        assert!(out.contains("inner = { path = \"crates/inner\", version = \"0.2.0\" }"));
        assert!(
            toml::from_str::<toml::Value>(toml).is_err(),
            "the fixture itself must be invalid as a whole file, or this test proves nothing"
        );
    }

    #[test]
    fn current_version_prefers_the_package_table_when_both_exist() {
        let toml = "[workspace.package]\nversion = \"9.9.9\"\n\n[package]\nversion = \"0.8.0\"\n";
        assert_eq!(current_version(toml).unwrap(), "0.8.0");
    }

    /// Same shape as `kanadehq/kanade`'s root `Cargo.toml`: no `[package]`
    /// at all, only `[workspace]` and `[workspace.package]`.
    #[test]
    fn current_version_falls_back_to_workspace_package_without_a_package_table() {
        let toml = "\
[workspace]\n\
members = [\"crates/a\", \"crates/b\"]\n\
\n\
[workspace.package]\n\
version = \"0.45.18\"\n";
        assert_eq!(current_version(toml).unwrap(), "0.45.18");
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

    #[tokio::test]
    async fn a_contended_lock_is_retried_until_the_holder_releases_it() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("bump").join("deadbeefdeadbeef.json");
        let held = MarkerLock::acquire(&marker)
            .unwrap()
            .expect("seed the contention");
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(held);
        });
        let waited =
            wait_for_marker_lock_with(&marker, Duration::from_millis(5), Duration::from_secs(5))
                .await
                .unwrap();
        assert!(
            waited.is_some(),
            "a merge landing behind another's still-running decision must not be dropped - it \
             must wait for that decision to finish and then judge against what it left behind"
        );
        releaser.await.unwrap();
    }

    #[tokio::test]
    async fn a_lock_held_past_the_ceiling_gives_up() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("bump").join("deadbeefdeadbeef.json");
        let _held = MarkerLock::acquire(&marker).unwrap().unwrap();
        let waited =
            wait_for_marker_lock_with(&marker, Duration::from_millis(2), Duration::from_millis(10))
                .await
                .unwrap();
        assert!(
            waited.is_none(),
            "a lock genuinely held past the ceiling must eventually give up rather than wait \
             forever"
        );
    }

    #[test]
    fn level_between_reads_off_the_differing_digit() {
        assert_eq!(
            level_between(
                Version::parse("0.8.0").unwrap(),
                Version::parse("1.0.0").unwrap()
            ),
            Some(BumpLevel::Major)
        );
        assert_eq!(
            level_between(
                Version::parse("0.8.0").unwrap(),
                Version::parse("0.9.0").unwrap()
            ),
            Some(BumpLevel::Minor)
        );
        assert_eq!(
            level_between(
                Version::parse("0.8.0").unwrap(),
                Version::parse("0.8.1").unwrap()
            ),
            Some(BumpLevel::Patch)
        );
        assert_eq!(
            level_between(
                Version::parse("0.8.0").unwrap(),
                Version::parse("0.8.0").unwrap()
            ),
            None
        );
    }

    #[test]
    fn open_release_pr_is_found_among_unrelated_pull_requests() {
        let json = r#"[
            {"url": "https://example.invalid/pull/1", "headRefName": "feat/something"},
            {"url": "https://example.invalid/pull/2", "headRefName": "chore/release-v0.9.0"}
        ]"#;
        let found = parse_open_release_pr(json).unwrap();
        assert_eq!(
            found,
            Some((
                "chore/release-v0.9.0".to_owned(),
                "https://example.invalid/pull/2".to_owned()
            ))
        );
    }

    #[test]
    fn no_open_release_pr_reads_as_none_not_an_error() {
        let json =
            r#"[{"url": "https://example.invalid/pull/1", "headRefName": "feat/something"}]"#;
        assert_eq!(parse_open_release_pr(json).unwrap(), None);
        assert_eq!(parse_open_release_pr("[]").unwrap(), None);
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
