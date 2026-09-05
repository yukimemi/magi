//! Run configuration: the agent roster, the shape of the graph, and the
//! blindness / verification policy.
//!
//! Discovery order (first hit wins):
//!
//! 1. `--config <path>`
//! 2. `<repo>/magi.toml`
//! 3. `<repo>/.magi/config.toml`
//! 4. `<config_dir>/magi/config.toml`
//! 5. built-in defaults, with the agent roster derived from the agent CLIs
//!    actually installed on this machine
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

/// Which CLI drives an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// Anthropic Claude Code (`claude -p`).
    Claude,
    /// opencode (`opencode run`).
    Opencode,
    /// Antigravity CLI (`agy -p`). Gemini CLI is deliberately absent: Google
    /// retired the standalone client for individual accounts in favour of this
    /// one, so an adapter for it would be dead code on a live machine.
    Antigravity,
    /// Arbitrary command. The escape hatch, and what the test suite drives.
    Command,
}

impl AgentKind {
    /// Executable that must be on `PATH` for this kind, if any.
    pub fn program(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("claude"),
            Self::Opencode => Some("opencode"),
            Self::Antigravity => Some("agy"),
            Self::Command => None,
        }
    }

    /// Lowercase name as written in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Opencode => "opencode",
            Self::Antigravity => "antigravity",
            Self::Command => "command",
        }
    }
}

/// How the prompt reaches the agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Piped on stdin.
    Stdin,
    /// Passed as a positional argument. Beware OS command-line limits.
    Argv,
    /// Written to a file; the agent is told to read it. No length limit.
    File,
}

/// One addressable agent in the roster.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpec {
    /// Stable identifier used by `[roles]` and by the stats tables.
    pub id: String,
    /// Which CLI to drive.
    pub kind: AgentKind,
    /// Model passed through to the CLI (`--model` / `-m`). CLI default if unset.
    #[serde(default)]
    pub model: Option<String>,
    /// `kind = "command"` only: argv. Supports `{prompt_file}`, `{cwd}`,
    /// `{label}`, `{session}` placeholders.
    #[serde(default)]
    pub command: Vec<String>,
    /// Extra arguments appended to the built command line.
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Extra environment variables for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Override the per-kind prompt delivery default.
    #[serde(default)]
    pub prompt_delivery: Option<Delivery>,
}

impl AgentSpec {
    /// Default prompt delivery for this agent.
    ///
    /// `opencode` and `agy` take the prompt as an argument, which on Windows
    /// caps out around 32 KiB — well under a judging prompt carrying three
    /// patches — so both get a file instead.
    pub fn delivery(&self) -> Delivery {
        self.prompt_delivery.unwrap_or(match self.kind {
            AgentKind::Claude | AgentKind::Command => Delivery::Stdin,
            AgentKind::Opencode | AgentKind::Antigravity => Delivery::File,
        })
    }

    /// Human-facing label, e.g. `opus (claude:opus)`.
    pub fn display(&self) -> String {
        match &self.model {
            Some(m) => format!("{} ({}:{m})", self.id, self.kind.as_str()),
            None => format!("{} ({})", self.id, self.kind.as_str()),
        }
    }
}

/// Explicit role assignment. Empty lists are filled in by
/// [`Config::resolve_roles`] by rotating the roster.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Roles {
    /// Agents that implement the task, one worktree each.
    pub implementers: Vec<String>,
    /// Agents that rank the candidates blind.
    pub judges: Vec<String>,
    /// Agents that review the winning patch.
    pub reviewers: Vec<String>,
    /// Agent that applies review findings. Defaults to the winner's author.
    pub fixer: Option<String>,
    /// Agent that runs the `magi plan` interview and the browser conversation.
    ///
    /// Unset picks a `claude` seat, else the first runnable agent in roster
    /// order - which is roster *order*, not a judgement about who interviews
    /// well. Naming one here is worth it because the interview is the one node
    /// a human sits through: the model that asks good questions is not
    /// necessarily the one that writes the best patch, and on a phone there is
    /// no `--agent` to type.
    pub planner: Option<String>,
}

/// Graph shape and limits.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Graph {
    /// Parallel implementations of the same task.
    pub candidates: usize,
    /// Independent judges.
    pub judges: usize,
    /// Deliberation rounds when the judges' first choices disagree.
    pub deliberate_rounds: usize,
    /// Reviewers per review round.
    pub reviewers: usize,
    /// Maximum review+fix rounds before the run is declared blocked.
    pub review_rounds: usize,
    /// Maximum agent processes running at once.
    pub max_parallel: usize,
    /// Language for the prose the agents write (`en` / `ja` / any language name).
    pub language: String,
    /// Keep one CLI conversation per seat, so a judge remembers its own
    /// argument across deliberation rounds and the fixer remembers its own
    /// implementation across review rounds.
    ///
    /// Sessions are scoped to a *seat*, never to an agent id: the same model
    /// sitting as implementer and as judge gets two unrelated conversations,
    /// which is what keeps blind judging blind.
    pub sessions: bool,
    /// Per-node timeouts, seconds.
    pub timeout_implement: u64,
    /// Per-node timeouts, seconds.
    pub timeout_judge: u64,
    /// Per-node timeouts, seconds.
    pub timeout_review: u64,
    /// Per-node timeouts, seconds.
    pub timeout_fix: u64,
    /// Retries for an agent invocation that fails or returns nothing usable.
    pub retries: usize,
    /// Root for candidate / judge worktrees. Defaults to `~/wt/magi`.
    pub worktree_root: Option<PathBuf>,
    /// After the pull request is open, keep going: watch its checks and
    /// reviews, run a fix round when they are unhappy, and ask to merge.
    ///
    /// On, because stopping at an open pull request left the operator doing
    /// the watching by hand - six times in the session this was built in - and
    /// that is the work the loop exists to take. It only engages for
    /// `merge = "pr"`; every other merge mode ends the run as before.
    ///
    /// Turning this on does **not** hand magi the merge button:
    /// [`Graph::land_approval`] is on too, and nothing merges without an
    /// explicit answer. Setting both to their non-defaults is the only way to
    /// get an unattended merge, and it has to be chosen twice.
    pub land: bool,
    /// Land rounds - watch, fix, push - before the run is left for a human.
    pub land_rounds: usize,
    /// Ask the owner before merging, showing what is about to land.
    ///
    /// On, and it is what makes `land` safe to have on: the question carries a
    /// rendered panel - the diffstat, the patch, the checks, the review
    /// comments that were addressed, and the subject the squash will use - so
    /// the decision is made on evidence rather than on trust, from wherever
    /// the operator happens to be.
    ///
    /// Silence is a hold. An unanswered approval never merges, and neither
    /// does any answer other than the word `merge`.
    pub land_approval: bool,
    /// How long to wait for an owner to answer a question before the run is
    /// abandoned, seconds. A parked run costs nothing, so this is generous;
    /// it exists so a forgotten question cannot pin a worktree forever.
    pub answer_timeout: u64,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            candidates: 3,
            judges: 3,
            deliberate_rounds: 1,
            reviewers: 2,
            review_rounds: 6,
            max_parallel: 4,
            language: "en".to_owned(),
            sessions: true,
            timeout_implement: 3600,
            timeout_judge: 1200,
            timeout_review: 1200,
            timeout_fix: 1800,
            retries: 1,
            worktree_root: None,
            land: true,
            land_rounds: 4,
            land_approval: true,
            answer_timeout: 86_400,
        }
    }
}

/// What to do when vendor-identifying text is found in material shown to judges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LeakPolicy {
    /// Record the leak, show the patch unmodified.
    Warn,
    /// Replace the token with `[REDACTED]` in the presented patch.
    Redact,
    /// Abort the run.
    Fail,
}

/// Blindness policy.
///
/// Commit messages and candidate summaries are *always* stripped of
/// attribution trailers and redacted — that is where signatures actually
/// appear. [`Blind::on_leak`] governs the patch body only, where blanket
/// redaction would corrupt the artifact under judgement.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Blind {
    /// Install a per-worktree `commit-msg` hook that deletes attribution
    /// trailers before they can land in a candidate's history.
    pub commit_msg_hook: bool,
    /// Literal, case-insensitive substrings. A line containing any of them is
    /// dropped from commit messages and summaries; the `commit-msg` hook is
    /// generated from the same list.
    pub strip_lines: Vec<String>,
    /// Case-insensitive substrings that identify a vendor or model.
    pub vendor_tokens: Vec<String>,
    /// Policy for vendor tokens found in the patch body.
    pub on_leak: LeakPolicy,
    /// Seed for label assignment and per-judge presentation order. Derived from
    /// the run id when unset; set it to make a run reproducible.
    pub seed: Option<u64>,
}

impl Default for Blind {
    fn default() -> Self {
        Self {
            commit_msg_hook: true,
            strip_lines: [
                "Co-Authored-By:",
                "Signed-off-by:",
                "Assisted-by:",
                "Generated-by:",
                "Generated with",
                "\u{1f916}",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            vendor_tokens: [
                "claude",
                "anthropic",
                "codex",
                "openai",
                "chatgpt",
                "gemini",
                "grok",
                "xai",
                "copilot",
                "opencode",
                "qoder",
                "cursor",
                "\u{1f916}",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            on_leak: LeakPolicy::Warn,
            seed: None,
        }
    }
}

/// Shell commands that gate the winner.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Verify {
    /// Run in the winner's worktree once per review round. Its output is fed
    /// back to the fixer. This is the "real machine" leg of the review.
    pub e2e: Vec<String>,
    /// Final gate. Must all exit 0 before a merge is attempted.
    pub gate: Vec<String>,
    /// Shell used to run the commands above. Defaults to `sh -c`, or
    /// `cmd /C` when `sh` is not on `PATH`.
    pub shell: Option<Vec<String>>,
}

/// What to do with the winning branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMode {
    /// Leave the branch alone and print the merge command.
    None,
    /// `git merge --no-ff` into the base branch in the primary worktree.
    Local,
    /// Push the branch and open a PR with `gh pr create`.
    Pr,
}

/// Merge policy.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Merge {
    /// Default is [`MergeMode::None`]: magi never touches your base branch
    /// unless you ask it to.
    pub mode: MergeMode,
    /// Base branch. Defaults to the branch checked out when the run started.
    pub base: Option<String>,
    /// Remote for `mode = "pr"`.
    pub remote: String,
}

impl Default for Merge {
    fn default() -> Self {
        Self {
            mode: MergeMode::None,
            base: None,
            remote: "origin".to_owned(),
        }
    }
}

/// How magi keeps itself current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateMode {
    /// Never check.
    Off,
    /// Check in the background and print a one-line banner when a newer
    /// release exists.
    Notify,
    /// Check and install silently.
    Install,
}

/// Self-update policy.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Update {
    /// Default is [`UpdateMode::Notify`]: magi tells you, and lets you decide.
    pub mode: UpdateMode,
    /// Minimum time between checks, e.g. `24h`. kaishin's default when unset.
    pub interval: Option<String>,
}

impl Default for Update {
    fn default() -> Self {
        Self {
            mode: UpdateMode::Notify,
            interval: None,
        }
    }
}

/// Top-level configuration.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Agent roster.
    pub agents: Vec<AgentSpec>,
    /// Role assignment.
    pub roles: Roles,
    /// Graph shape.
    pub graph: Graph,
    /// Blindness policy.
    pub blind: Blind,
    /// Verification commands.
    pub verify: Verify,
    /// Merge policy.
    pub merge: Merge,
    /// Self-update policy.
    pub update: Update,
    /// Project-specific text appended to the node prompts.
    pub prompts: Prompts,
    /// How the operator is told a run is waiting on them.
    pub notify: Notify,
    /// Local repositories the plan surface can start or derive a conversation
    /// against.
    pub repos: Repos,
}

/// Where `magi plan` and the browser interview look for a repository other
/// than the one they were started against.
///
/// `roots` is an array, so per [`array_keys`] it can only be declared in one
/// config layer - the machine layer, since which checkouts exist on disk is a
/// *machine* fact in the same way the agent roster is: a repository's own
/// `magi.toml` cannot state where its siblings live before magi has resolved
/// which repository to read that file from in the first place.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Repos {
    /// Roots to scan for a ghq-layout checkout: `<root>/<host>/<owner>/<repo>`
    /// with a `.git` directory. Empty by default - nothing is scanned unless
    /// asked to be.
    pub roots: Vec<PathBuf>,
    /// How long a scan is trusted before the next request re-scans it,
    /// seconds. `0` means never trust it: scan on every request. Defaults to
    /// a day, the same order of magnitude as [`Graph::answer_timeout`] for
    /// the same reason - a checkout does not usually appear or vanish inside
    /// a session, so there is little to gain from scanning more often than
    /// that, and an explicit refresh exists for the moment one does.
    pub scan_ttl: u64,
}

impl Default for Repos {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            scan_ttl: 86_400,
        }
    }
}

/// Project-specific text appended to each node's prompt.
///
/// **Additive by construction.** These fields cannot replace magi's prompts,
/// only extend them, and that restriction is the whole design. The built-in
/// prompts carry the invariants the competition rests on: a judging prompt
/// names no authors, every structured answer must arrive as one fenced `json`
/// block, and a judge is told not to speculate about who wrote what. A config
/// that could overwrite them would let a typo silently un-blind the panel or
/// break the parser, and the symptom would be "the judges got worse" rather
/// than an error.
///
/// Repository-wide context belongs in `AGENTS.md`, which every agent already
/// reads from the checkout. Use these fields for the things a *magi node*
/// needs to know and a repository file cannot say - for instance that
/// reviewers here should ignore formatting because a hook owns it.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Prompts {
    /// Appended to every node's prompt.
    pub all: String,
    /// Appended for implementers.
    pub implement: String,
    /// Appended for judges, both ranking and voting.
    pub judge: String,
    /// Appended for reviewers.
    pub review: String,
    /// Appended for the fixer.
    pub fix: String,
}

impl Prompts {
    /// The overlay for one node, or `None` when nothing is configured.
    ///
    /// `node` is the graph's own node name, so a new node gets no overlay
    /// rather than the wrong one.
    pub fn overlay(&self, node: &str) -> Option<String> {
        let specific = match node {
            "implement" => &self.implement,
            "judge" | "vote" | "deliberate" => &self.judge,
            "review" => &self.review,
            "fix" => &self.fix,
            _ => "",
        };
        let mut parts: Vec<&str> = Vec::new();
        for p in [self.all.trim(), specific.trim()] {
            if !p.is_empty() {
                parts.push(p);
            }
        }
        if parts.is_empty() {
            return None;
        }
        Some(parts.join("\n\n"))
    }
}

/// How the operator is told that a run is waiting on them.
///
/// A command rather than a built-in integration: magi is one binary with no
/// network dependencies, and every operator's notification path is different -
/// ntfy, a Slack webhook, a Windows toast, an SSH to a machine that beeps.
/// Shelling out keeps all of them possible and none of them magi's problem.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Notify {
    /// Command and arguments. `{summary}`, `{run}` and `{url}` are replaced.
    /// Empty means no notification - the web UI is then the only surface.
    pub command: Vec<String>,
}

/// Roles resolved to concrete agent specs for one run.
#[derive(Debug, Clone)]
pub struct ResolvedRoles {
    /// One per candidate.
    pub implementers: Vec<AgentSpec>,
    /// One per judge.
    pub judges: Vec<AgentSpec>,
    /// One per reviewer slot.
    pub reviewers: Vec<AgentSpec>,
    /// Explicit fixer, if configured.
    pub fixer: Option<AgentSpec>,
}

/// Every array-valued key in a config table, as a dotted path.
///
/// Dotted so the error names `roles.implementers` rather than `implementers`:
/// an operator with three config files needs to know which key, not just that
/// there was one. `vars` is skipped because it is teravars' own input, merged
/// on purpose and never deserialised into `Config`.
fn array_keys(table: &toml::value::Table, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in table {
        if prefix.is_empty() && k == "vars" {
            continue;
        }
        let path = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Array(_) => out.push(path),
            toml::Value::Table(t) => out.extend(array_keys(t, &path)),
            _ => {}
        }
    }
    out
}

impl Config {
    /// Load one file through teravars: Tera rendering, `[vars]` resolution,
    /// and the `include = [...]` directive.
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_layers(&[path.to_path_buf()])
    }

    /// Load and deep-merge a stack of config files, later files winning.
    ///
    /// This is why the config is TOML-through-teravars rather than plain serde:
    /// the roster is a *machine* fact (which CLIs and plans you pay for) while
    /// the gate is a *repository* fact (`cargo make check` here, `pnpm test`
    /// there). Picking one file and ignoring the other would force every repo
    /// to restate the roster.
    pub fn load_layers(paths: &[PathBuf]) -> Result<Self> {
        let mut engine = teravars::Engine::default();
        let mut ctx = teravars::system_context();
        // teravars ships `system.*` and `vars`; `env` is left to the consumer.
        // A config that has to name a shared build-cache directory or a
        // machine-specific path needs it, so magi provides it as a map:
        // `{{ env.NAME | default(value='...') }}`.
        let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
        ctx.insert("env", &env);
        if let Some(last) = paths.last()
            && let Some(dir) = last.parent()
        {
            ctx.insert("repo", &dir.to_string_lossy());
            ctx.insert(
                "repo_name",
                &dir.file_name().unwrap_or_default().to_string_lossy(),
            );
        }
        if paths.len() > 1 {
            Self::refuse_split_arrays(paths, &mut engine, &ctx)?;
        }
        let merged = teravars::load_merged(paths, &mut engine, &ctx).with_context(|| {
            format!(
                "rendering config via teravars: {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let mut table = merged.config;
        // `[vars]` is teravars' own input, already resolved into the render
        // context; `deny_unknown_fields` must not trip over it.
        table.remove("vars");
        toml::Value::Table(table)
            .try_into()
            .context("deserializing magi config")
    }

    /// Refuse an array that two layers both declare.
    ///
    /// teravars **appends** arrays when it merges layers, and that is wrong for
    /// every array magi has: `implementers` is an ordered list of seats,
    /// `verify.gate` is the commands to run, `notify.command` is an argv.
    /// Concatenating two of them yields something nobody wrote - three
    /// implementers out of a machine's two and a repository's one, or an argv
    /// of `["ntfy", "publish", "curl", "-X"]`.
    ///
    /// Replacing instead would be the right merge rule, but the rule lives in
    /// teravars, which several other projects depend on; changing it there is
    /// a decision for that crate, not something to fake here by re-reading the
    /// files with different semantics and hoping the two paths agree.
    ///
    /// So magi refuses the ambiguity rather than resolving it silently. The
    /// cost of guessing is a roster the operator did not ask for and is paying
    /// for by the token.
    fn refuse_split_arrays(
        paths: &[PathBuf],
        engine: &mut teravars::Engine,
        ctx: &teravars::Context,
    ) -> Result<()> {
        let mut seen: std::collections::BTreeMap<String, PathBuf> = Default::default();
        for path in paths {
            let one = teravars::load_merged([path], engine, ctx)
                .with_context(|| format!("rendering {}", path.display()))?;
            for key in array_keys(&one.config, "") {
                if let Some(first) = seen.get(&key) {
                    bail!(
                        "`{key}` is an array declared in two config layers:\n  \
                         {}\n  {}\nteravars appends arrays when it merges, so \
                         magi would run the concatenation of both - which is \
                         not what either file says. Declare `{key}` in exactly \
                         one of them.",
                        first.display(),
                        path.display()
                    );
                }
                seen.insert(key, path.clone());
            }
        }
        Ok(())
    }

    /// Every config layer that applies to `repo`, in increasing precedence.
    pub fn layers(repo: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(dir) = dirs::config_dir() {
            paths.push(dir.join("magi").join("config.toml"));
        }
        paths.push(repo.join(".magi").join("config.toml"));
        paths.push(repo.join("magi.toml"));
        paths.retain(|p| p.is_file());
        paths
    }

    /// Resolve the config for `repo`, honouring an explicit `--config` path.
    ///
    /// Returns the config and the layers it came from, empty for built-in
    /// defaults.
    pub fn discover(repo: &Path, explicit: Option<&Path>) -> Result<(Self, Vec<PathBuf>)> {
        if let Some(p) = explicit {
            let paths = vec![p.to_path_buf()];
            return Ok((Self::load_layers(&paths)?, paths));
        }
        let paths = Self::layers(repo);
        if paths.is_empty() {
            return Ok((Self::autodetected(), paths));
        }
        Ok((Self::load_layers(&paths)?, paths))
    }

    /// Built-in config whose roster is the agent CLIs found on `PATH`.
    pub fn autodetected() -> Self {
        let mut cfg = Self::default();
        for (kind, id, model) in [
            (AgentKind::Claude, "opus", Some("opus")),
            (AgentKind::Claude, "sonnet", Some("sonnet")),
            (AgentKind::Antigravity, "antigravity", None),
            (AgentKind::Opencode, "opencode", None),
        ] {
            if kind.program().is_some_and(which) && !cfg.agents.iter().any(|a| a.id == id) {
                cfg.agents.push(AgentSpec {
                    id: id.to_owned(),
                    kind,
                    model: model.map(str::to_owned),
                    command: Vec::new(),
                    extra_args: Vec::new(),
                    env: BTreeMap::new(),
                    prompt_delivery: None,
                });
            }
        }
        cfg
    }

    /// Look an agent up by id.
    pub fn agent(&self, id: &str) -> Result<&AgentSpec> {
        self.agents
            .iter()
            .find(|a| a.id == id)
            .with_context(|| format!("no agent with id `{id}` in the roster"))
    }

    /// Fill the roles out to the configured widths.
    ///
    /// An empty role list rotates through the whole roster, so a three-agent
    /// roster with `candidates = 3` gives one implementation per agent, and
    /// `judges = 3` rotates the judge seats by one so that judge *i* is not the
    /// author of candidate *i* whenever the roster has more than one agent.
    pub fn resolve_roles(&self) -> Result<ResolvedRoles> {
        if self.agents.is_empty() {
            bail!(
                "agent roster is empty: no agent CLI found on PATH and no \
                 [[agents]] in the config. Run `magi init` to write a starter \
                 magi.toml."
            );
        }
        let pick = |ids: &[String], count: usize, offset: usize| -> Result<Vec<AgentSpec>> {
            let mut out = Vec::with_capacity(count);
            for i in 0..count {
                let spec = if ids.is_empty() {
                    self.agents[(i + offset) % self.agents.len()].clone()
                } else {
                    self.agent(&ids[i % ids.len()])?.clone()
                };
                out.push(spec);
            }
            Ok(out)
        };
        Ok(ResolvedRoles {
            implementers: pick(&self.roles.implementers, self.graph.candidates, 0)?,
            judges: pick(&self.roles.judges, self.graph.judges, 1)?,
            reviewers: pick(&self.roles.reviewers, self.graph.reviewers, 0)?,
            fixer: self
                .roles
                .fixer
                .as_deref()
                .map(|f| self.agent(f).cloned())
                .transpose()?,
        })
    }

    /// Shell prefix for [`Verify`] commands.
    pub fn shell(&self) -> Vec<String> {
        if let Some(s) = &self.verify.shell {
            return s.clone();
        }
        if which("sh") {
            vec!["sh".to_owned(), "-c".to_owned()]
        } else {
            vec!["cmd".to_owned(), "/C".to_owned()]
        }
    }

    /// Starter config, as written by `magi init`.
    pub fn starter_toml() -> String {
        let detected = Self::autodetected();
        let mut s = String::from(
            "# magi — blind multi-agent implementation competition.\n\
             # `magi run \"<task>\"` walks: implement (N parallel worktrees)\n\
             #   -> blind judging -> deliberation -> private final vote\n\
             #   -> fold losers -> review + E2E loop -> gate -> merge.\n\
             #\n\
             # Rendered by teravars: a `[vars]` table, env\n\
             # and system lookups, and `include = [...]` all work. Tera\n\
             # braces are live everywhere in this file, but comments are\n\
             # stripped before rendering (teravars >= 0.2.2), so a comment\n\
             # may quote `{{ ... }}` freely.\n\
             #\n\
             # Layers deep-merge in increasing\n\
             # precedence, so the roster can live once per machine in\n\
             # <config_dir>/magi/config.toml and each repo only states its own\n\
             # gate:\n\
             #   <config_dir>/magi/config.toml  <  .magi/config.toml  <  magi.toml\n\n\
             [vars]\n\
             # Reference it as vars.cache inside Tera braces, anywhere below.\n\
             # Single quotes inside the braces: teravars renders the raw file\n\
             # text, so TOML's own \\\" escaping never reaches Tera.\n\
             cache = \"{{ env.MAGI_CACHE | default(value='/tmp') }}\"\n\n",
        );
        if detected.agents.is_empty() {
            s.push_str(
                "# No agent CLI was found on PATH. Fill this in by hand.\n\
                 # kind = claude | opencode | antigravity | command\n\
                 [[agents]]\nid = \"opus\"\nkind = \"claude\"\nmodel = \"opus\"\n\n",
            );
        } else {
            for a in &detected.agents {
                s.push_str("[[agents]]\n");
                s.push_str(&format!("id = {:?}\n", a.id));
                s.push_str(&format!("kind = {:?}\n", a.kind.as_str()));
                if let Some(m) = &a.model {
                    s.push_str(&format!("model = {m:?}\n"));
                }
                s.push('\n');
            }
        }
        s.push_str(
            "# Leave a role list empty to rotate through the roster.\n\
             [roles]\n\
             implementers = []\n\
             judges = []\n\
             reviewers = []\n\n\
             [graph]\n\
             candidates = 3\n\
             judges = 3\n\
             deliberate_rounds = 1\n\
             reviewers = 2\n\
             review_rounds = 6\n\
             max_parallel = 4\n\
             language = \"en\"\n\
             # One CLI conversation per seat: judges keep their own argument\n\
             # across deliberation, the fixer keeps its implementation context.\n\
             sessions = true\n\n\
             [verify]\n\
             # Run once per review round in the winner's worktree; failures are\n\
             # fed back to the fixer.\n\
             e2e = []\n\
             # Final gate. Every command must exit 0 before a merge.\n\
             gate = []\n\n\
             [merge]\n\
             # none | local | pr\n\
             mode = \"none\"\n\n\
             [update]\n\
             # off | notify | install — checked in the background, throttled.\n\
             mode = \"notify\"\n\
             # interval = \"24h\"\n",
        );
        s
    }
}

/// Is `program` on `PATH`?
pub fn which(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let exts: Vec<String> = std::env::var("PATHEXT")
        .map(|v| v.split(';').map(|e| e.to_lowercase()).collect())
        .unwrap_or_default();
    std::env::split_paths(&paths).any(|dir| {
        let direct = dir.join(program);
        if direct.is_file() {
            return true;
        }
        exts.iter().any(|ext| {
            let mut name = program.to_owned();
            name.push_str(ext);
            dir.join(name).is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            kind: AgentKind::Command,
            model: None,
            command: vec!["true".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }
    }

    #[test]
    fn empty_roles_rotate_judges_off_their_own_candidate() {
        let cfg = Config {
            agents: vec![spec("a"), spec("b"), spec("c")],
            ..Config::default()
        };
        let roles = cfg.resolve_roles().unwrap();
        let impls: Vec<&str> = roles.implementers.iter().map(|a| a.id.as_str()).collect();
        let judges: Vec<&str> = roles.judges.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(impls, ["a", "b", "c"]);
        assert_eq!(judges, ["b", "c", "a"]);
        for (i, j) in judges.iter().enumerate() {
            assert_ne!(*j, impls[i], "judge {i} must not sit on its own candidate");
        }
    }

    #[test]
    fn single_agent_roster_fills_every_seat() {
        let cfg = Config {
            agents: vec![spec("solo")],
            ..Config::default()
        };
        let roles = cfg.resolve_roles().unwrap();
        assert_eq!(roles.implementers.len(), 3);
        assert!(roles.judges.iter().all(|a| a.id == "solo"));
    }

    #[test]
    fn explicit_roles_win() {
        let cfg = Config {
            agents: vec![spec("a"), spec("b")],
            roles: Roles {
                implementers: vec!["b".to_owned()],
                judges: vec!["a".to_owned()],
                reviewers: Vec::new(),
                fixer: Some("a".to_owned()),
                ..Roles::default()
            },
            ..Config::default()
        };
        let roles = cfg.resolve_roles().unwrap();
        assert!(roles.implementers.iter().all(|a| a.id == "b"));
        assert!(roles.judges.iter().all(|a| a.id == "a"));
        assert_eq!(roles.fixer.unwrap().id, "a");
    }

    #[test]
    fn unknown_agent_id_is_an_error() {
        let cfg = Config {
            agents: vec![spec("a")],
            roles: Roles {
                judges: vec!["nope".to_owned()],
                ..Roles::default()
            },
            ..Config::default()
        };
        assert!(cfg.resolve_roles().is_err());
    }

    #[test]
    fn empty_roster_is_an_error() {
        assert!(Config::default().resolve_roles().is_err());
    }

    #[test]
    fn repos_default_to_no_roots_and_a_day_of_trust() {
        assert_eq!(Config::default().repos.roots, Vec::<PathBuf>::new());
        assert_eq!(Config::default().repos.scan_ttl, 86_400);
    }

    #[test]
    fn a_config_file_with_no_repos_table_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magi.toml");
        std::fs::write(&path, "[graph]\ncandidates = 2\n").unwrap();
        let cfg = Config::load(&path).expect("must load without [repos]");
        assert_eq!(cfg.repos.roots, Vec::<PathBuf>::new());
        assert_eq!(cfg.repos.scan_ttl, 86_400);
    }

    #[test]
    fn starter_toml_loads_through_teravars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magi.toml");
        std::fs::write(&path, Config::starter_toml()).unwrap();
        let parsed = Config::load(&path).expect("starter config must load");
        assert_eq!(parsed.graph.candidates, 3);
        assert_eq!(parsed.merge.mode, MergeMode::None);
        assert!(parsed.graph.sessions);
        assert_eq!(parsed.update.mode, UpdateMode::Notify);
    }

    #[test]
    fn later_layers_win_and_vars_render() {
        let dir = tempfile::tempdir().unwrap();
        let machine = dir.path().join("machine.toml");
        let project = dir.path().join("magi.toml");
        // The machine layer owns the roster...
        std::fs::write(
            &machine,
            "[[agents]]\nid = \"opus\"\nkind = \"claude\"\nmodel = \"opus\"\n\n\
             [graph]\ncandidates = 3\nmax_parallel = 8\n",
        )
        .unwrap();
        // ...and the project layer only states what is repo-specific, plus a
        // `[vars]` value interpolated into a command.
        std::fs::write(
            &project,
            "[vars]\ncache = \"/shared\"\n\n\
             [graph]\ncandidates = 2\n\n\
             [verify]\ngate = [\"CARGO_TARGET_DIR={{ vars.cache }}/t cargo test\"]\n",
        )
        .unwrap();

        let cfg = Config::load_layers(&[machine, project]).expect("layered load");
        assert_eq!(cfg.agents.len(), 1, "roster comes from the machine layer");
        assert_eq!(cfg.graph.candidates, 2, "project layer wins");
        assert_eq!(cfg.graph.max_parallel, 8, "machine layer survives");
        assert_eq!(
            cfg.verify.gate,
            ["CARGO_TARGET_DIR=/shared/t cargo test".to_owned()]
        );
    }

    #[test]
    fn env_is_available_to_templates_with_a_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magi.toml");
        // teravars ships no `env`; magi adds it, and the `default` filter has
        // to cover the unset case or every machine would need the variable.
        //
        // Deliberately no named variable: `env` is keyed by the exact spelling
        // the OS reports, and Windows says `Path` where POSIX says `PATH`, so a
        // test asserting `env.PATH` passes on one runner and fails on another.
        // The map's non-emptiness is the platform-neutral claim.
        std::fs::write(
            &path,
            "[verify]\n\
             gate = [\"cache={{ env.MAGI_TEST_UNSET_XYZ | default(value='fallback') }}\", \
             \"populated={{ env | length > 0 }}\"]\n",
        )
        .unwrap();
        let cfg = Config::load(&path).expect("env lookup must render");
        assert_eq!(cfg.verify.gate[0], "cache=fallback");
        assert_eq!(cfg.verify.gate[1], "populated=true");
    }

    #[test]
    fn a_broken_template_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magi.toml");
        std::fs::write(&path, "[graph]\nlanguage = \"{{ nope.\"\n").unwrap();
        let err = Config::load(&path).expect_err("must not silently ignore");
        assert!(err.to_string().contains("teravars"), "{err}");
    }

    #[test]
    fn tera_syntax_in_comments_is_inert() {
        // teravars >= 0.2.2 strips `#` comments before Tera sees the file, so a
        // comment may quote template syntax without rendering. Before 0.2.2 this
        // load failed: the commented-out braces reached the template parser.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magi.toml");
        std::fs::write(
            &path,
            "# a comment may quote templates: `{{ env.NOPE | default(value='x') }}` and `{% if %}`\n\
             [graph]\ncandidates = 2\n",
        )
        .unwrap();
        let cfg = Config::load(&path).expect("comments must be inert, not rendered");
        assert_eq!(cfg.graph.candidates, 2);
    }

    #[test]
    fn opencode_defaults_to_file_delivery() {
        let mut s = spec("oc");
        s.kind = AgentKind::Opencode;
        assert_eq!(s.delivery(), Delivery::File);
        s.prompt_delivery = Some(Delivery::Argv);
        assert_eq!(s.delivery(), Delivery::Argv);
    }
    #[test]
    fn the_land_loop_is_on_but_it_cannot_merge_without_being_asked() {
        // Both default on, and that pair is the safety property: `land` takes
        // over the watching an operator was doing by hand, `land_approval`
        // keeps the irreversible step a human decision. An unattended merge
        // needs BOTH flipped, which has to be chosen deliberately twice.
        let g = Graph::default();
        assert!(
            g.land,
            "stopping at an open PR left the watching to a human"
        );
        assert!(
            g.land_approval,
            "on-by-default land is only defensible while this is also on"
        );
        assert!(g.land_rounds > 0, "a loop with no budget never terminates");
    }
    #[test]
    fn an_array_declared_in_two_layers_is_refused_instead_of_concatenated() {
        // teravars appends arrays. For an ordered list of seats, or an argv,
        // the concatenation is something neither file says - and the operator
        // pays for the extra seats by the token.
        let dir = tempfile::tempdir().unwrap();
        let machine = dir.path().join("machine.toml");
        let repo = dir.path().join("magi.toml");
        std::fs::write(&machine, "[roles]\nimplementers = [\"a\", \"b\"]\n").unwrap();
        std::fs::write(&repo, "[roles]\nimplementers = [\"oc\"]\n").unwrap();

        let err = Config::load_layers(&[machine.clone(), repo.clone()])
            .expect_err("two layers naming one array must not merge silently")
            .to_string();
        assert!(err.contains("roles.implementers"), "{err}");
        // Both files are named: the fix is to delete one of them, and the
        // operator has to know which two to choose between.
        assert!(err.contains("machine.toml"), "{err}");
        assert!(err.contains("magi.toml"), "{err}");
    }

    #[test]
    fn a_scalar_in_one_layer_and_an_array_in_another_still_merges() {
        // The split the layering exists for: state a preference machine-wide,
        // let the repository own its own lists.
        let dir = tempfile::tempdir().unwrap();
        let machine = dir.path().join("machine.toml");
        let repo = dir.path().join("magi.toml");
        std::fs::write(&machine, "[roles]\nplanner = \"opus\"\n").unwrap();
        std::fs::write(
            &repo,
            "[[agents]]\nid = \"oc\"\nkind = \"opencode\"\n\n\
             [roles]\nimplementers = [\"oc\"]\n",
        )
        .unwrap();

        let cfg = Config::load_layers(&[machine, repo]).expect("layers merge");
        assert_eq!(cfg.roles.planner.as_deref(), Some("opus"));
        assert_eq!(cfg.roles.implementers, ["oc"]);
        assert_eq!(cfg.agents.len(), 1, "the roster is not doubled");
    }
}
