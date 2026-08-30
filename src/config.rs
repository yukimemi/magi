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
             # Rendered by teravars, comments included: a `[vars]` table, env\n\
             # and system lookups, and `include = [...]` all work. Note that\n\
             # Tera braces are live everywhere in this file, so do not write\n\
             # them in a comment unless you mean them.\n\
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
    fn opencode_defaults_to_file_delivery() {
        let mut s = spec("oc");
        s.kind = AgentKind::Opencode;
        assert_eq!(s.delivery(), Delivery::File);
        s.prompt_delivery = Some(Delivery::Argv);
        assert_eq!(s.delivery(), Delivery::Argv);
    }
}
