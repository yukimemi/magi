//! Structured answers extracted from free-form agent output.
//!
//! Agents are asked to end with a fenced `json` block. They mostly do, and
//! sometimes they narrate afterwards, emit two blocks, or wrap the object in
//! prose. [`extract_json`] therefore scans for *every* balanced top-level
//! object in the text and returns the last one that deserializes into the type
//! the caller wants, rather than trusting a single regex to find the right one.
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A judge's independent ranking of the candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ranking {
    /// Labels, best first. Must be a permutation of the presented labels.
    pub ranking: Vec<String>,
    /// Per-label justification.
    #[serde(default)]
    pub reasons: BTreeMap<String, String>,
    /// Self-reported confidence, 1-5.
    #[serde(default)]
    pub confidence: Option<u8>,
    /// Per-label strengths worth porting onto the winner, written for the
    /// candidates the judge did not rank first. Optional: a judge that
    /// names none simply leaves the graft phase nothing to do.
    #[serde(default, deserialize_with = "lenient_strengths")]
    pub strengths: BTreeMap<String, Vec<String>>,
}

/// Read `strengths` without ever failing: this field is optional, so a value of
/// the wrong shape (`null`, a string, a map of plain strings) must cost the
/// judge nothing but the strengths. Entries that are not a string array keep
/// their string members only; anything else is dropped.
fn lenient_strengths<'de, D>(d: D) -> Result<BTreeMap<String, Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(d)?;
    let mut out = BTreeMap::new();
    if let serde_json::Value::Object(map) = value {
        for (label, v) in map {
            match v {
                serde_json::Value::Array(items) => {
                    let list: Vec<String> = items
                        .into_iter()
                        .filter_map(|i| i.as_str().map(str::to_owned))
                        .collect();
                    if !list.is_empty() {
                        out.insert(label, list);
                    }
                }
                serde_json::Value::String(s) if !s.trim().is_empty() => {
                    out.insert(label, vec![s]);
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

impl Ranking {
    /// The judge's first choice.
    pub fn top(&self) -> Option<&str> {
        self.ranking.first().map(String::as_str)
    }

    /// Reject a ranking that is not a permutation of `labels`, so a malformed
    /// verdict is retried instead of silently skewing the tally.
    pub fn validate(&self, labels: &[char]) -> Result<()> {
        let mut got: Vec<char> = self
            .ranking
            .iter()
            .filter_map(|s| s.trim().chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        got.sort_unstable();
        got.dedup();
        let mut want: Vec<char> = labels.to_vec();
        want.sort_unstable();
        if got != want {
            bail!(
                "ranking {:?} is not a permutation of the candidate labels {:?}",
                self.ranking,
                labels
            );
        }
        Ok(())
    }

    /// Normalise labels to single uppercase characters.
    pub fn normalized(&self) -> Vec<char> {
        self.ranking
            .iter()
            .filter_map(|s| s.trim().chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }
}

/// A judge's final vote, collected privately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalVote {
    /// The winning label, in this judge's view.
    pub vote: String,
    /// Why.
    #[serde(default)]
    pub reason: String,
}

impl FinalVote {
    /// The voted label as a single uppercase char.
    pub fn label(&self) -> Option<char> {
        self.vote
            .trim()
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
    }
}

/// How bad a review finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Cosmetic.
    Nit,
    /// Should fix, does not block.
    Minor,
    /// Should fix before merge.
    Major,
    /// Must fix before merge.
    Blocker,
}

impl Severity {
    /// Does this finding hold the merge?
    pub fn blocks(self) -> bool {
        matches!(self, Self::Major | Self::Blocker)
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Nit => "nit",
            Self::Minor => "minor",
            Self::Major => "major",
            Self::Blocker => "blocker",
        })
    }
}

/// One review finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Assigned by magi after parsing, e.g. `R1-1-2`. Never trusted from the
    /// agent, because the fixer's adoption report is keyed by it.
    #[serde(default)]
    pub id: String,
    /// How bad.
    pub severity: Severity,
    /// File it concerns.
    #[serde(default)]
    pub file: Option<String>,
    /// Line it concerns.
    #[serde(default)]
    pub line: Option<u32>,
    /// One-line summary.
    pub title: String,
    /// The argument.
    #[serde(default)]
    pub detail: String,
}

/// A reviewer seat's verdict on the patch, cast alongside its findings.
///
/// Three values, not a boolean, because "fine to proceed" and "fine, but
/// look at these" are different signals for the operator watching the run —
/// collapsing them would hide exactly the middle case a lens-based reviewer
/// is most likely to land on. Ord follows declaration order (least to most
/// cautious) so a round's [`ReviewVote::worst`] is a plain `max`, the same
/// trick [`Severity`] uses for `blocks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVote {
    /// No reservations.
    Approve,
    /// Fine to proceed, but the findings below are worth fixing.
    ApproveWithFindings,
    /// Do not proceed as-is.
    Reject,
}

impl ReviewVote {
    /// Operator-facing label, used in events and the report — never
    /// `Debug`, so a rename of a variant does not silently reword history.
    pub fn label(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::ApproveWithFindings => "approve with findings",
            Self::Reject => "reject",
        }
    }

    /// The most cautious of a set of votes, or `None` if none were cast.
    ///
    /// A round's recorded verdict must never read softer than its most
    /// cautious seat — the same reason a single blocking finding, not an
    /// average of severities, decides whether a round is clean.
    pub fn worst(votes: impl IntoIterator<Item = Self>) -> Option<Self> {
        votes.into_iter().max()
    }
}

/// A reviewer's report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    /// Findings, worst first is conventional but not required.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Reviewer's overall verdict prose.
    #[serde(default)]
    pub summary: String,
    /// This seat's vote. Required, not defaulted: a reviewer that skips it
    /// is retried the same as one that skipped `severity` on a finding,
    /// rather than silently counted as an `approve`.
    pub vote: ReviewVote,
}

/// A reviewer seat's revote after a split round, cast once it has read every
/// seat's findings and votes (still numbered, never named — see
/// [`crate::prompt::review_reconsider`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRevote {
    /// The seat's revote.
    pub vote: ReviewVote,
    /// Why, one or two sentences.
    #[serde(default)]
    pub reason: String,
}

/// The fixer's response to a round of findings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixReport {
    /// Finding ids that were acted on.
    #[serde(default)]
    pub addressed: Vec<String>,
    /// Finding ids that were deliberately not acted on, with the reason.
    #[serde(default)]
    pub rejected: Vec<Rejection>,
    /// What changed.
    #[serde(default)]
    pub notes: String,
}

/// A finding the fixer declined, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rejection {
    /// Finding id.
    pub id: String,
    /// Argument for not acting.
    #[serde(default)]
    pub why: String,
}

/// A judge's position during deliberation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// Where the judge currently stands.
    #[serde(default)]
    pub tentative: Option<String>,
}

/// One advisor's independent design proposal, gathered by
/// `graph::Runner::advise` before a run's implementer seats begin.
///
/// No patch and no plan of tool calls - a design proposal is cheap exactly
/// because it is a few paragraphs an agent can write without touching the
/// repository, which is what makes running several of them, on every run,
/// affordable in a way several full implementations were not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    /// What to do and how, in the advisor's own words.
    pub approach: String,
    /// The one tradeoff this design turns on.
    pub key_tradeoff: String,
    /// What could go wrong.
    #[serde(default)]
    pub risks: Vec<String>,
    /// Files or modules the design touches.
    #[serde(default)]
    pub touches: Vec<String>,
    /// Why this earns its complexity over the obvious first draft - the
    /// question that keeps a proposal from being a restatement of the task.
    pub why_not_naive: String,
}

impl Proposal {
    /// Reject a proposal with an empty field that matters, so a malformed or
    /// lazy answer is retried instead of silently thinning the panel down to
    /// prose nobody can act on.
    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("approach", &self.approach),
            ("key_tradeoff", &self.key_tradeoff),
            ("why_not_naive", &self.why_not_naive),
        ] {
            if value.trim().is_empty() {
                bail!("`{field}` is empty");
            }
        }
        Ok(())
    }
}

/// Extract the last balanced JSON object in `text` that parses as `T`.
///
/// Handles fenced blocks, trailing prose, and multiple objects. Strings and
/// escapes are tracked so a `}` inside a string literal does not close an
/// object early.
pub fn extract_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escaped = false;
        let mut j = i;
        while j < bytes.len() {
            let c = bytes[j];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            spans.push((i, j + 1));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        // Skip past this object's opening brace either way; a truncated object
        // must not make the scan quadratic on long transcripts.
        i = if depth == 0 && j < bytes.len() {
            j + 1
        } else {
            i + 1
        };
    }

    let mut last_err = None;
    for (start, end) in spans.iter().rev() {
        match serde_json::from_str::<T>(&text[*start..*end]) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => bail!("no JSON object in the reply matched the expected shape: {e}"),
        None => bail!("the reply contained no JSON object"),
    }
}

/// Pull the text after a `## <heading>` marker, to the end or the next heading.
///
/// Used for the prose sections agents are asked to emit alongside their JSON.
pub fn section(text: &str, heading: &str) -> Option<String> {
    let want = heading.to_ascii_lowercase();
    let mut out: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("##") {
            let name = rest.trim_start_matches('#').trim().to_ascii_lowercase();
            if name == want {
                out = Some(String::new());
                continue;
            }
            if out.is_some() {
                break;
            }
            continue;
        }
        if let Some(buf) = out.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    out.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// The marker `prompt::implement`'s reply format documents for an
/// implementer that investigated and concluded, with concrete positive
/// evidence, that no code change belongs in this worktree — see that
/// prompt's "Reply format" section for the structured JSON schema required
/// after it.
pub const NO_CHANGE_NEEDED_MARKER: &str = "NO CHANGE NEEDED:";

/// Declared investigation status in a structured `NO CHANGE NEEDED:` report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoopStatus {
    /// Every requirement was verified to already be satisfied with nothing left unresolved.
    AlreadySatisfied,
    /// Investigation did not finish or could not verify satisfaction.
    Incomplete,
    /// Investigation or verification failed.
    Failed,
}

/// Concrete category of verification evidence cited by a [`NoopCheck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoopEvidenceKind {
    /// A git commit SHA present in the repository's base history.
    Commit,
    /// A test or test suite executed to confirm the behavior.
    Test,
    /// A command executed to verify repository state or behavior.
    Command,
    /// A repository-relative path inspected in the worktree.
    Path,
}

impl NoopEvidenceKind {
    /// Stable lower-case name matching the wire JSON spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Test => "test",
            Self::Command => "command",
            Self::Path => "path",
        }
    }
}

impl fmt::Display for NoopEvidenceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One requirement evaluated by the implementer and the passing check ids that
/// prove it is already satisfied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoopRequirement {
    /// Short statement of the requirement checked.
    pub requirement: String,
    /// Whether this requirement is already satisfied.
    pub satisfied: bool,
    /// Check ids (referencing [`NoopCheck::id`]) proving satisfaction.
    pub evidence: Vec<String>,
}

/// One concrete verification check actually carried out during investigation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoopCheck {
    /// Unique identifier referenced by [`NoopRequirement::evidence`].
    pub id: String,
    /// Evidence kind: `commit`, `test`, `command`, or `path`.
    pub kind: NoopEvidenceKind,
    /// What was inspected or executed (commit SHA, test name, command, or repo-relative path).
    pub target: String,
    /// How verification was performed.
    pub method: String,
    /// Concrete observed output or finding.
    pub observed: String,
    /// Whether this check succeeded.
    pub passed: bool,
}

/// Structured JSON payload required immediately after [`NO_CHANGE_NEEDED_MARKER`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedNoopReport {
    /// Overall investigation outcome (`already_satisfied`, `incomplete`, or `failed`).
    pub status: NoopStatus,
    /// Per-requirement satisfaction assessments referencing concrete checks.
    pub requirements: Vec<NoopRequirement>,
    /// Ordered list of concrete verification checks performed.
    pub checks: Vec<NoopCheck>,
    /// Remaining unresolved issues; must be empty for `already_satisfied`.
    pub unresolved: Vec<String>,
}

impl VerifiedNoopReport {
    /// Enforce all structural and semantic invariants of an evidence-backed
    /// no-change declaration.
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self.status {
            NoopStatus::AlreadySatisfied => {}
            NoopStatus::Incomplete => {
                return Err(
                    "status is `incomplete`; investigation did not establish that no change is needed"
                        .to_owned(),
                );
            }
            NoopStatus::Failed => {
                return Err("status is `failed`; investigation did not succeed".to_owned());
            }
        }

        if !self.unresolved.is_empty() {
            let items: Vec<&str> = self
                .unresolved
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                return Err(format!(
                    "unresolved list is non-empty ({} blank entry/entries)",
                    self.unresolved.len()
                ));
            }
            return Err(format!("unresolved item(s) remain: {}", items.join("; ")));
        }

        if self.checks.is_empty() {
            return Err("no verification checks were provided".to_owned());
        }

        let mut checks_by_id: BTreeMap<String, &NoopCheck> = BTreeMap::new();
        for check in &self.checks {
            let id = check.id.trim();
            if id.is_empty() {
                return Err("a verification check has an empty `id`".to_owned());
            }
            let id_key = id.to_ascii_lowercase();
            if checks_by_id.contains_key(&id_key) {
                return Err(format!("duplicate check id `{id}`"));
            }

            let target = check.target.trim();
            if target.is_empty() || is_placeholder_value(target) {
                return Err(format!(
                    "check `{id}` has empty or placeholder `target` (`{target}`)"
                ));
            }
            let method = check.method.trim();
            if method.is_empty() || is_placeholder_value(method) || is_placeholder_prose(method) {
                return Err(format!(
                    "check `{id}` has empty or placeholder `method` (`{method}`)"
                ));
            }
            let observed = check.observed.trim();
            if observed.is_empty()
                || is_placeholder_value(observed)
                || is_placeholder_prose(observed)
            {
                return Err(format!(
                    "check `{id}` has empty or placeholder `observed` (`{observed}`)"
                ));
            }

            match check.kind {
                NoopEvidenceKind::Commit if !is_commit_sha(target) => {
                    return Err(format!(
                        "check `{id}` (`commit`) target `{target}` must be a 7-40 character hex commit SHA"
                    ));
                }
                NoopEvidenceKind::Path
                    if !is_safe_repo_relative_path(strip_path_locator(target)) =>
                {
                    return Err(format!(
                        "check `{id}` (`path`) target `{target}` must be a repository-relative path"
                    ));
                }
                _ => {}
            }

            if check.passed {
                if let Some(phrase) = incomplete_admission(method) {
                    return Err(format!(
                        "check `{id}` claims `passed: true` but `method` admits incomplete work (`{phrase}`)"
                    ));
                }
                if let Some(phrase) = incomplete_admission(observed) {
                    return Err(format!(
                        "check `{id}` claims `passed: true` but `observed` admits incomplete work (`{phrase}`)"
                    ));
                }
            }

            checks_by_id.insert(id_key, check);
        }

        if let Some(last) = self.checks.last()
            && !last.passed
        {
            return Err(format!(
                "final verification check `{}` did not pass",
                last.id.trim()
            ));
        }

        if self.requirements.is_empty() {
            return Err("no requirements were evaluated".to_owned());
        }

        let mut seen_reqs = BTreeSet::new();
        for req in &self.requirements {
            let text = req.requirement.trim();
            if text.is_empty() || is_placeholder_value(text) {
                return Err(format!(
                    "requirement entry has empty or placeholder description (`{text}`)"
                ));
            }
            let req_key = normalize_whitespace_lower(text);
            if !seen_reqs.insert(req_key) {
                return Err(format!("duplicate requirement `{text}`"));
            }
            if !req.satisfied {
                return Err(format!("requirement `{text}` is not satisfied"));
            }
            if req.evidence.is_empty() {
                return Err(format!("requirement `{text}` has no supporting evidence"));
            }

            let mut seen_ev = BTreeSet::new();
            for raw_id in &req.evidence {
                let ev_id = raw_id.trim();
                if ev_id.is_empty() {
                    return Err(format!(
                        "requirement `{text}` contains an empty evidence id"
                    ));
                }
                let ev_key = ev_id.to_ascii_lowercase();
                if !seen_ev.insert(ev_key.clone()) {
                    return Err(format!(
                        "requirement `{text}` references duplicate check id `{ev_id}`"
                    ));
                }
                let Some(check) = checks_by_id.get(&ev_key) else {
                    return Err(format!(
                        "requirement `{text}` references unknown check `{ev_id}`"
                    ));
                };
                if !check.passed {
                    return Err(format!(
                        "contradictory evidence: requirement `{text}` cites failed check `{ev_id}`"
                    ));
                }
            }
        }

        Ok(())
    }

    /// Unique checks referenced by [`Self::requirements`], in check order.
    pub fn cited_checks(&self) -> Vec<&NoopCheck> {
        let cited: BTreeSet<String> = self
            .requirements
            .iter()
            .flat_map(|r| r.evidence.iter())
            .map(|id| id.trim().to_ascii_lowercase())
            .collect();
        self.checks
            .iter()
            .filter(|c| cited.contains(&c.id.trim().to_ascii_lowercase()))
            .collect()
    }

    /// Concise, single-line human-readable evidence summary for reports and
    /// diagnostics.
    pub fn summary_text(&self) -> String {
        let checks_by_id: BTreeMap<String, &NoopCheck> = self
            .checks
            .iter()
            .map(|c| (c.id.trim().to_ascii_lowercase(), c))
            .collect();
        let parts: Vec<String> = self
            .requirements
            .iter()
            .map(|req| {
                let ev: Vec<String> = req
                    .evidence
                    .iter()
                    .filter_map(|id| checks_by_id.get(&id.trim().to_ascii_lowercase()))
                    .map(|c| {
                        format!(
                            "{} {}: {}",
                            c.kind,
                            collapse_ws(c.target.trim()),
                            collapse_ws(c.observed.trim())
                        )
                    })
                    .collect();
                format!(
                    "{} [{}]",
                    collapse_ws(req.requirement.trim()),
                    ev.join("; ")
                )
            })
            .collect();
        format!("already satisfied: {}", parts.join(" | "))
    }
}

/// Strip an optional trailing line/range locator (`:123`, `:123-145`, `:123:4`,
/// `#L123`, `#L123-L145`) from a repository-relative path reference.
pub fn strip_path_locator(target: &str) -> &str {
    let target = target.trim();
    if let Some((path, frag)) = target.rsplit_once("#L")
        && !path.is_empty()
        && is_line_range_fragment(frag)
    {
        return path;
    }
    if let Some((path, suffix)) = target.rsplit_once(':')
        && !path.is_empty()
        && !(path.len() == 1 && path.as_bytes()[0].is_ascii_alphabetic())
    {
        // Check `path:line:col` before `path:line`, since `:col` is also all digits.
        if !suffix.is_empty()
            && suffix.bytes().all(|b| b.is_ascii_digit())
            && let Some((base, line)) = path.rsplit_once(':')
            && !base.is_empty()
            && !(base.len() == 1 && base.as_bytes()[0].is_ascii_alphabetic())
            && !line.is_empty()
            && line.bytes().all(|b| b.is_ascii_digit())
        {
            return base;
        }
        if is_colon_line_suffix(suffix) {
            return path;
        }
    }
    target
}

fn is_line_range_fragment(frag: &str) -> bool {
    if let Some((a, b)) = frag.split_once('-') {
        let b = b.strip_prefix('L').unwrap_or(b);
        !a.is_empty()
            && !b.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && b.bytes().all(|c| c.is_ascii_digit())
    } else {
        !frag.is_empty() && frag.bytes().all(|c| c.is_ascii_digit())
    }
}

fn is_colon_line_suffix(suffix: &str) -> bool {
    if let Some((a, b)) = suffix.split_once('-') {
        !a.is_empty()
            && !b.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && b.bytes().all(|c| c.is_ascii_digit())
    } else {
        !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit())
    }
}

fn is_commit_sha(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_safe_repo_relative_path(s: &str) -> bool {
    if s.is_empty() || s.contains('\0') {
        return false;
    }
    if s.starts_with('/') || s.starts_with('\\') {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return false;
    }
    let mut has_normal = false;
    for comp in Path::new(s).components() {
        match comp {
            Component::Normal(_) => has_normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    has_normal
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_whitespace_lower(s: &str) -> String {
    collapse_ws(s).to_ascii_lowercase()
}

fn is_placeholder_value(s: &str) -> bool {
    let norm = normalize_whitespace_lower(s);
    matches!(
        norm.as_str(),
        "" | "-"
            | "--"
            | "..."
            | "?"
            | "???"
            | "n/a"
            | "na"
            | "none"
            | "null"
            | "nil"
            | "tbd"
            | "todo"
            | "unknown"
            | "pending"
            | "unchecked"
            | "unverified"
            | "not run"
            | "not checked"
            | "not tested"
            | "skipped"
            | "omitted"
            | "placeholder"
    )
}

fn is_placeholder_prose(s: &str) -> bool {
    let norm = normalize_whitespace_lower(s);
    matches!(
        norm.as_str(),
        "ok" | "okay"
            | "pass"
            | "passed"
            | "passing"
            | "true"
            | "yes"
            | "done"
            | "checked"
            | "verified"
            | "confirmed"
            | "satisfied"
            | "same"
            | "lgtm"
            | "looks good"
            | "works"
            | "good"
    )
}

/// Auxiliary defense against passing checks whose prose explicitly admits
/// incomplete or unverified work. Kept narrow and word-boundary-aware so
/// legitimate reports describing resolved past failures (e.g. "previously
/// failed; now passes with 0 failed") or symbol names (e.g.
/// `has_unconfirmed_command`) are never rejected.
fn incomplete_admission(s: &str) -> Option<&'static str> {
    const PHRASES: &[&str] = &[
        "not established",
        "remains unresolved",
        "still unresolved",
        "left unresolved",
        "not verified",
        "not confirmed",
        "could not verify",
        "unable to verify",
        "failed to verify",
        "could not confirm",
        "unable to confirm",
        "failed to confirm",
        "could not complete",
        "unable to complete",
        "did not complete",
        "not completed",
        "was not completed",
        "were not completed",
        "did not run",
        "was not run",
        "were not run",
        "not executed",
        "skipped verification",
        "investigation incomplete",
        "incomplete investigation",
        "no verification checks",
        "no reproduction checks",
        "no checks were completed",
        "no checks were run",
        "inconclusive",
    ];
    let norm = normalize_whitespace_lower(s);
    PHRASES
        .iter()
        .copied()
        .find(|phrase| contains_whole_phrase(&norm, phrase))
}

fn contains_whole_phrase(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let end = abs + needle.len();
        let before_ok = abs == 0 || !is_word(bytes[abs - 1]);
        let after_ok = end == bytes.len() || !is_word(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Strip at most one outer Markdown code fence (` ``` ` or ` ```json `) with no
/// trailing content after the closing fence.
fn strip_single_json_fence(body: &str) -> std::result::Result<&str, String> {
    let trimmed = body.trim();
    if !trimmed.starts_with("```") {
        return Ok(trimmed);
    }
    let Some((first_line, rest)) = trimmed.split_once('\n') else {
        return Err("unterminated fenced JSON block after NO CHANGE NEEDED:".to_owned());
    };
    let lang = first_line.trim().trim_start_matches('`').trim();
    if !lang.is_empty() && !lang.eq_ignore_ascii_case("json") {
        return Err(format!(
            "unexpected fenced block language `{lang}`; expected a JSON object"
        ));
    }
    let rest_trimmed = rest.trim_end();
    let Some(inner) = rest_trimmed.strip_suffix("```") else {
        return Err(
            "unterminated fenced JSON block or trailing prose after closing fence".to_owned(),
        );
    };
    if !inner.is_empty() && !inner.ends_with('\n') && !inner.ends_with('\r') {
        return Err("closing fence must sit on its own line".to_owned());
    }
    Ok(inner.trim())
}

/// Strictly parse and validate a `NO CHANGE NEEDED:` declaration from an
/// implementer's `## SUMMARY` section.
///
/// - Returns `None` when the summary does not open with
///   [`NO_CHANGE_NEEDED_MARKER`] (an ordinary change summary or plain prose
///   explanation).
/// - Returns `Some(Ok(report))` only when the payload is a single strict JSON
///   object matching [`VerifiedNoopReport`] and passing
///   [`VerifiedNoopReport::validate`].
/// - Returns `Some(Err(reason))` with a concrete rejection reason when the
///   summary opened with the marker but failed parsing or validation (legacy
///   freeform prose, incomplete/failed status, unresolved items, missing or
///   failed checks, placeholders, duplicate keys/ids, or contradictory claims).
pub fn parse_verified_noop(
    summary: &str,
) -> Option<std::result::Result<VerifiedNoopReport, String>> {
    let rest = summary.trim_start().strip_prefix(NO_CHANGE_NEEDED_MARKER)?;
    let body = rest.trim();
    if body.is_empty() {
        return Some(Err(
            "empty NO CHANGE NEEDED declaration with no structured JSON evidence".to_owned(),
        ));
    }
    let json_text = match strip_single_json_fence(body) {
        Ok(s) => s,
        Err(e) => return Some(Err(e)),
    };
    if !json_text.starts_with('{') || !json_text.ends_with('}') {
        return Some(Err(
            "expected a single JSON object after NO CHANGE NEEDED:; prose-only or legacy declarations are rejected"
                .to_owned(),
        ));
    }
    let report: VerifiedNoopReport = match serde_json::from_str(json_text) {
        Ok(r) => r,
        Err(e) => {
            return Some(Err(format!(
                "invalid structured JSON after NO CHANGE NEEDED:: {e}"
            )));
        }
    };
    if let Err(reason) = report.validate() {
        return Some(Err(reason));
    }
    Some(Ok(report))
}

/// Pull the validated evidence summary out of an implementer's `## SUMMARY`
/// section when it opens with [`NO_CHANGE_NEEDED_MARKER`] and passes strict
/// structured validation.
///
/// Returns `None` for an ordinary bullet-list summary, and also for any
/// malformed, legacy prose-only, incomplete, contradictory, or unsupported
/// declaration. Whether the declaration is trusted against repository state and
/// CLI command evidence is [`crate::graph`]'s adoption guard's call.
pub fn verified_noop(summary: &str) -> Option<String> {
    parse_verified_noop(summary)?
        .ok()
        .map(|report| report.summary_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_has_explicit_operator_labels() {
        for (severity, label) in [
            (Severity::Nit, "nit"),
            (Severity::Minor, "minor"),
            (Severity::Major, "major"),
            (Severity::Blocker, "blocker"),
        ] {
            assert_eq!(severity.to_string(), label);
        }
    }

    #[test]
    fn malformed_strengths_never_cost_the_ranking() {
        for bad in [
            "null",
            r#"{"B":"one point"}"#,
            r#""x""#,
            r#"[1]"#,
            r#"{"B":[1,"ok"]}"#,
        ] {
            let text = format!(r#"{{"ranking":["A","B"],"strengths":{bad}}}"#);
            let r: Ranking = extract_json(&text).unwrap();
            assert_eq!(r.ranking, ["A", "B"]);
        }
    }

    #[test]
    fn fenced_block_is_found() {
        let text = "Here is my verdict.\n\n```json\n{\"ranking\":[\"B\",\"A\"]}\n```\n";
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.top(), Some("B"));
    }

    #[test]
    fn last_matching_object_wins_over_an_earlier_example() {
        let text = concat!(
            "The format is {\"ranking\":[\"X\"]} for illustration.\n",
            "```json\n{\"ranking\":[\"C\",\"A\",\"B\"],\"confidence\":4}\n```\n",
            "Happy to elaborate.\n"
        );
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.normalized(), ['C', 'A', 'B']);
        assert_eq!(r.confidence, Some(4));
    }

    #[test]
    fn braces_inside_strings_do_not_close_the_object() {
        let text = r#"{"ranking":["A"],"reasons":{"A":"uses format!(\"{}\", x) safely}"}}"#;
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.top(), Some("A"));
        assert!(r.reasons["A"].contains("format!"));
    }

    #[test]
    fn objects_of_the_wrong_shape_are_skipped() {
        let text = concat!(
            "```json\n{\"ranking\":[\"A\",\"B\"]}\n```\n",
            "and some telemetry: {\"tokens\":123}\n"
        );
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.normalized(), ['A', 'B']);
    }

    #[test]
    fn no_json_is_an_error_not_a_default() {
        let err = extract_json::<Ranking>("I decline to produce JSON.").unwrap_err();
        assert!(err.to_string().contains("no JSON object"));
    }

    #[test]
    fn truncated_object_does_not_hang() {
        let err = extract_json::<Ranking>("{\"ranking\": [\"A\"").unwrap_err();
        assert!(err.to_string().contains("no JSON object"));
    }

    #[test]
    fn ranking_validation_rejects_a_non_permutation() {
        let r = Ranking {
            ranking: vec!["A".to_owned(), "A".to_owned()],
            reasons: BTreeMap::new(),
            confidence: None,
            strengths: BTreeMap::new(),
        };
        assert!(r.validate(&['A', 'B', 'C']).is_err());

        let r = Ranking {
            ranking: vec!["c".to_owned(), "B".to_owned(), "A".to_owned()],
            reasons: BTreeMap::new(),
            confidence: None,
            strengths: BTreeMap::new(),
        };
        r.validate(&['A', 'B', 'C']).expect("case is normalised");
        assert_eq!(r.normalized(), ['C', 'B', 'A']);
    }

    #[test]
    fn final_vote_label_is_normalised() {
        let v: FinalVote = extract_json(r#"{"vote":" b ","reason":"tests"}"#).unwrap();
        assert_eq!(v.label(), Some('B'));
    }

    #[test]
    fn severity_blocking_is_major_and_up() {
        assert!(Severity::Blocker.blocks());
        assert!(Severity::Major.blocks());
        assert!(!Severity::Minor.blocks());
        assert!(!Severity::Nit.blocks());
        assert!(Severity::Blocker > Severity::Nit);
    }

    #[test]
    fn review_parses_with_optional_fields_missing() {
        let r: Review = extract_json(
            r#"{"vote":"reject","findings":[{"severity":"blocker","title":"panics on empty input"}]}"#,
        )
        .unwrap();
        assert_eq!(r.findings.len(), 1);
        assert!(r.findings[0].file.is_none());
        assert_eq!(r.findings[0].id, "");
        assert_eq!(r.vote, ReviewVote::Reject);
    }

    #[test]
    fn review_without_a_vote_is_rejected_rather_than_defaulted() {
        let err = extract_json::<Review>(r#"{"findings":[]}"#).unwrap_err();
        assert!(err.to_string().contains("no JSON object"), "{err}");
    }

    #[test]
    fn review_vote_worst_is_the_most_cautious() {
        assert_eq!(
            ReviewVote::worst([ReviewVote::Approve, ReviewVote::Reject, ReviewVote::Approve]),
            Some(ReviewVote::Reject)
        );
        assert_eq!(
            ReviewVote::worst([ReviewVote::Approve, ReviewVote::ApproveWithFindings]),
            Some(ReviewVote::ApproveWithFindings)
        );
        assert_eq!(ReviewVote::worst(Vec::<ReviewVote>::new()), None);
    }

    #[test]
    fn review_revote_parses_the_reconsideration_shape() {
        let r: ReviewRevote =
            extract_json(r#"{"vote":"approve","reason":"the other findings do not hold"}"#)
                .unwrap();
        assert_eq!(r.vote, ReviewVote::Approve);
        assert_eq!(r.reason, "the other findings do not hold");
    }

    #[test]
    fn fix_report_parses_rejections() {
        let f: FixReport = extract_json(
            r#"{"addressed":["R1-1-1"],"rejected":[{"id":"R1-2-1","why":"not reachable"}]}"#,
        )
        .unwrap();
        assert_eq!(f.addressed, ["R1-1-1"]);
        assert_eq!(f.rejected[0].id, "R1-2-1");
    }

    #[test]
    fn proposal_parses_with_optional_fields_missing() {
        let p: Proposal = extract_json(
            r#"{"approach":"do X","key_tradeoff":"simpler now, slower later","why_not_naive":"the naive version corrupts state under a retry"}"#,
        )
        .unwrap();
        assert!(p.risks.is_empty());
        assert!(p.touches.is_empty());
        p.validate()
            .expect("a proposal with only required fields is valid");
    }

    #[test]
    fn proposal_validation_rejects_an_empty_required_field() {
        let p = Proposal {
            approach: String::new(),
            key_tradeoff: "t".to_owned(),
            risks: Vec::new(),
            touches: Vec::new(),
            why_not_naive: "w".to_owned(),
        };
        let err = p.validate().unwrap_err();
        assert!(err.to_string().contains("approach"));
    }

    #[test]
    fn sections_are_sliced_by_heading() {
        let text = "## SUMMARY\nchanged the retry loop.\nadded a test.\n\n## NOTES\nignore me\n";
        assert_eq!(
            section(text, "summary").unwrap(),
            "changed the retry loop.\nadded a test."
        );
        assert_eq!(section(text, "notes").unwrap(), "ignore me");
        assert!(section(text, "missing").is_none());
    }

    const REGRESSION_QUOTED_SUMMARY: &str = "NO CHANGE NEEDED: Not established. No source changes, commit, or reproduction checks were completed. The reported Git configuration lock contention remains unresolved.";

    fn valid_noop_json() -> &'static str {
        r#"{"status":"already_satisfied","requirements":[{"requirement":"retry loop already fixed on base","satisfied":true,"evidence":["c1","c2"]}],"checks":[{"id":"c1","kind":"commit","target":"b32cfc4","method":"git merge-base --is-ancestor b32cfc4 HEAD","observed":"exit 0; b32cfc4 is on main and fixes the retry loop","passed":true},{"id":"c2","kind":"test","target":"retry_loop_recovers","method":"cargo test --lib retry_loop_recovers","observed":"test result: ok. 1 passed; 0 failed","passed":true}],"unresolved":[]}"#
    }

    #[test]
    fn structured_no_change_needed_report_yields_validated_evidence() {
        let inline = format!("NO CHANGE NEEDED: {}", valid_noop_json());
        let report = parse_verified_noop(&inline)
            .expect("marker present")
            .expect("valid structured report");
        assert_eq!(report.status, NoopStatus::AlreadySatisfied);
        let summary = verified_noop(&inline).expect("valid report yields summary");
        assert!(summary.contains("b32cfc4"), "{summary}");
        assert!(summary.contains("retry_loop_recovers"), "{summary}");

        let fenced = format!("NO CHANGE NEEDED:\n```json\n{}\n```\n", valid_noop_json());
        assert!(
            verified_noop(&fenced).is_some(),
            "single fenced json block must be accepted"
        );
    }

    #[test]
    fn operational_regression_quoted_response_is_rejected() {
        let err = parse_verified_noop(REGRESSION_QUOTED_SUMMARY)
            .expect("marker present")
            .expect_err("incomplete prose response must be rejected");
        assert!(err.contains("single JSON object"), "{err}");
        assert!(verified_noop(REGRESSION_QUOTED_SUMMARY).is_none());

        // Even if the exact regression text is stuffed into a valid-shaped JSON
        // object claiming `already_satisfied` and `passed: true`, auxiliary
        // contradiction detection rejects it.
        let stuffed = r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"resolve git lock contention","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"command","target":"git config --list","method":"ran investigation","observed":"Not established. No source changes, commit, or reproduction checks were completed. The reported Git configuration lock contention remains unresolved.","passed":true}],"unresolved":[]}"#;
        let err = parse_verified_noop(stuffed)
            .expect("marker present")
            .expect_err("stuffed incomplete admission must be rejected");
        assert!(err.contains("admits incomplete work"), "{err}");
        assert!(verified_noop(stuffed).is_none());
    }

    #[test]
    fn legacy_freeform_and_empty_no_change_declarations_are_rejected() {
        let legacy = "NO CHANGE NEEDED: already fixed by b32cfc4, which is on main \
                      (git merge-base --is-ancestor confirms it).";
        assert!(matches!(parse_verified_noop(legacy), Some(Err(_))));
        assert!(verified_noop(legacy).is_none());
        assert!(matches!(
            parse_verified_noop("NO CHANGE NEEDED:"),
            Some(Err(_))
        ));
        assert!(matches!(
            parse_verified_noop("NO CHANGE NEEDED:   \n  "),
            Some(Err(_))
        ));
    }

    #[test]
    fn an_ordinary_summary_is_not_verified() {
        assert!(parse_verified_noop("- changed the retry loop.\n- added a test.").is_none());
        assert!(verified_noop("- changed the retry loop.\n- added a test.").is_none());
        // The marker only counts leading the section, not mentioned in passing.
        assert!(
            parse_verified_noop("I considered NO CHANGE NEEDED: but wrote a fix instead.")
                .is_none()
        );
    }

    #[test]
    fn incomplete_failed_unresolved_or_unverified_reports_are_rejected() {
        let cases = [
            (
                "incomplete status",
                r#"NO CHANGE NEEDED: {"status":"incomplete","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"read file contents","passed":true}],"unresolved":[]}"#,
                "incomplete",
            ),
            (
                "failed status",
                r#"NO CHANGE NEEDED: {"status":"failed","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"read file contents","passed":true}],"unresolved":[]}"#,
                "failed",
            ),
            (
                "unresolved items",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"read file contents","passed":true}],"unresolved":["Git lock contention remains unresolved"]}"#,
                "unresolved",
            ),
            (
                "empty checks",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[],"unresolved":[]}"#,
                "no verification checks",
            ),
            (
                "empty requirements",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"read file contents","passed":true}],"unresolved":[]}"#,
                "no requirements",
            ),
            (
                "unsatisfied requirement",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":false,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"read file contents","passed":true}],"unresolved":[]}"#,
                "not satisfied",
            ),
            (
                "requirement citing failed check",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"test","target":"lock_test","method":"cargo test --lib lock_test","observed":"FAILED (exit 101)","passed":false},{"id":"c2","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"contains lock helper","passed":true}],"unresolved":[]}"#,
                "contradictory evidence",
            ),
            (
                "final check failed",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"contains lock helper","passed":true},{"id":"c2","kind":"test","target":"lock_test","method":"cargo test --lib lock_test","observed":"FAILED (exit 101)","passed":false}],"unresolved":[]}"#,
                "final verification check",
            ),
            (
                "unknown check reference",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c9"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"contains lock helper","passed":true}],"unresolved":[]}"#,
                "unknown check",
            ),
            (
                "placeholder observed",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"ok","passed":true}],"unresolved":[]}"#,
                "placeholder",
            ),
            (
                "duplicate check id",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"found guard","passed":true},{"id":"c1","kind":"path","target":"src/run.rs","method":"read src/run.rs","observed":"found struct","passed":true}],"unresolved":[]}"#,
                "duplicate check id",
            ),
            (
                "duplicate json key",
                r#"NO CHANGE NEEDED: {"status":"incomplete","status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"found guard","passed":true}],"unresolved":[]}"#,
                "duplicate field",
            ),
            (
                "unknown json field",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","extra":1,"requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"found guard","passed":true}],"unresolved":[]}"#,
                "unknown field",
            ),
            (
                "trailing second json object",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"src/graph.rs","method":"read src/graph.rs","observed":"found guard","passed":true}],"unresolved":[]} {"extra":true}"#,
                "trailing",
            ),
            (
                "non-sha commit target",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"commit","target":"HEAD","method":"git log -1 HEAD","observed":"commit exists on HEAD","passed":true}],"unresolved":[]}"#,
                "hex commit SHA",
            ),
            (
                "parent traversal path target",
                r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"req","satisfied":true,"evidence":["c1"]}],"checks":[{"id":"c1","kind":"path","target":"../outside.rs","method":"read file","observed":"found text","passed":true}],"unresolved":[]}"#,
                "repository-relative path",
            ),
        ];

        for (label, input, want) in cases {
            let err = parse_verified_noop(input)
                .unwrap_or_else(|| panic!("{label}: expected marker match"))
                .expect_err(&format!("{label}: expected rejection"));
            assert!(
                err.contains(want),
                "{label}: expected error containing `{want}`, got `{err}`"
            );
        }
    }

    #[test]
    fn resolved_past_failure_and_earlier_exploratory_failure_are_accepted() {
        let input = r#"NO CHANGE NEEDED: {"status":"already_satisfied","requirements":[{"requirement":"previous unconfirmed command bug is fixed","satisfied":true,"evidence":["c2","c3"]}],"checks":[{"id":"c1","kind":"command","target":"git show deadbeef","method":"git show deadbeef","observed":"initial lookup failed: old commit id was from another branch","passed":false},{"id":"c2","kind":"commit","target":"b32cfc4","method":"git merge-base --is-ancestor b32cfc4 HEAD","observed":"exit 0; previously failed lock issue was fixed by b32cfc4 on main","passed":true},{"id":"c3","kind":"test","target":"has_unconfirmed_command","method":"cargo test --lib has_unconfirmed_command","observed":"test result: ok. 3 passed; 0 failed","passed":true}],"unresolved":[]}"#;
        let report = parse_verified_noop(input)
            .expect("marker present")
            .expect("resolved past failure + passing final check must be accepted");
        assert_eq!(report.cited_checks().len(), 2);
        assert!(verified_noop(input).is_some());
    }

    #[test]
    fn path_locator_stripping_keeps_clean_relative_paths_and_rejects_drive_letters() {
        assert_eq!(strip_path_locator("src/graph.rs:123"), "src/graph.rs");
        assert_eq!(strip_path_locator("src/graph.rs:123-145"), "src/graph.rs");
        assert_eq!(strip_path_locator("src/graph.rs:123:4"), "src/graph.rs");
        assert_eq!(strip_path_locator("src/graph.rs#L123"), "src/graph.rs");
        assert_eq!(strip_path_locator("src/graph.rs#L10-L20"), "src/graph.rs");
        assert!(!is_safe_repo_relative_path(strip_path_locator(
            "C:/tmp/a.rs"
        )));
        assert!(!is_safe_repo_relative_path(strip_path_locator(
            "/etc/hosts"
        )));
        assert!(is_safe_repo_relative_path(strip_path_locator(
            "src/graph.rs:42"
        )));
    }
}
