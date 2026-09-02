//! Driving agent CLIs.
//!
//! Every agent in magi is a subscription CLI (`claude`, `opencode`, `agy`) or
//! an arbitrary command, invoked headless in a working directory. There is no
//! API-key path on purpose: the CLIs carry the operator's own plan, and they
//! are the only interface that exposes an agent's whole tool loop rather than a
//! single completion.
//!
//! # Seats, not agents
//!
//! Conversations are keyed by *seat* ([`SeatState::key`]), never by agent id. A
//! model that implements candidate B and also sits as judge 3 gets two
//! unrelated conversations, so the judge cannot recognise its own work from
//! having written it. Sessions are what make deliberation affordable — a judge
//! remembers its own argument instead of being re-fed the entire candidate set
//! — and seat scoping is what keeps that from destroying blindness.
//!
//! # Session mechanics per CLI
//!
//! | CLI | open | resume |
//! |-----|------|--------|
//! | `claude` | `--session-id <uuid>` (magi mints it) | `--resume <uuid>` |
//! | `opencode` | `--format json` reports `sessionID` | `-s <id>` |
//! | `agy` | `--output-format json` reports `conversation_id` | `--conversation <id>` |
//!
//! Claude is the only one magi can address before the first turn; the other two
//! report an id back, so [`SeatState::captured_session`] stays `None` until a
//! turn has completed and [`has_session`] answers honestly instead of
//! optimistically.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::config::{AgentKind, AgentSpec, Delivery};
use crate::rng::SplitMix64;

/// Conversation state for one seat, persisted with the run so `magi run
/// --resume` continues the same CLI conversations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatState {
    /// Stable seat name, e.g. `impl-A`, `judge-2`, `review-1`, `fix`.
    pub key: String,
    /// Agent id occupying the seat.
    pub agent: String,
    /// Turns already taken in this seat.
    pub turns: usize,
    /// Claude session uuid, minted up front so the first turn and every resume
    /// agree on it without parsing anything back.
    pub claude_session: Option<String>,
    /// Session id reported by a CLI that mints its own (`opencode`, `agy`).
    pub captured_session: Option<String>,
}

impl SeatState {
    /// New seat. `run_seed` scopes the minted Claude uuid to this run.
    pub fn new(key: &str, agent: &str, run_seed: u64) -> Self {
        let mut rng = SplitMix64::new(run_seed ^ crate::rng::fnv1a(key));
        Self {
            key: key.to_owned(),
            agent: agent.to_owned(),
            turns: 0,
            claude_session: Some(rng.uuid_v4()),
            captured_session: None,
        }
    }
}

/// Can a follow-up prompt rely on this seat remembering the conversation?
pub fn has_session(kind: AgentKind, seat: &SeatState, sessions_enabled: bool) -> bool {
    if !sessions_enabled || seat.turns == 0 {
        return false;
    }
    match kind {
        AgentKind::Claude => seat.claude_session.is_some(),
        AgentKind::Opencode | AgentKind::Antigravity => seat.captured_session.is_some(),
        AgentKind::Command => true,
    }
}

/// One agent invocation.
#[derive(Debug)]
pub struct Invocation<'a> {
    /// Working directory. Always a real checkout, so every CLI can read the
    /// repository without per-vendor "extra directory" flags.
    pub cwd: &'a Path,
    /// The full prompt.
    pub prompt: &'a str,
    /// Wall-clock limit; the process tree is killed when it elapses.
    pub timeout: Duration,
    /// May the agent modify files? Judges and reviewers may not.
    pub allow_write: bool,
    /// Continue this seat's conversation when the CLI supports it.
    pub sessions: bool,
    /// Directory for prompt / stdout / stderr artifacts.
    pub artifacts: &'a Path,
    /// Artifact filename stem.
    pub stem: &'a str,
}

/// Evidence that a CLI ran out of its rate limit / quota, distinct from an
/// ordinary failure.
///
/// `reset` is free text: CLIs render the reset time in their own locale, and
/// parsing it exactly would be a bug factory. When it is not readable we say
/// nothing rather than invent a format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Quota {
    /// Human-readable reset time, when the CLI printed one.
    #[serde(default)]
    pub reset: Option<String>,
}

/// Result of an agent invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    /// The agent's final message, extracted from whatever the CLI printed.
    pub text: String,
    /// Exit status code.
    pub exit_code: Option<i32>,
    /// Did the invocation hit its timeout?
    pub timed_out: bool,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// Artifact file names, relative to the run's `artifacts/` directory.
    pub artifacts: Vec<String>,
    /// Rate-limit / quota exhaustion, when it can be told apart from a normal
    /// failure. `None` for a normal failure, a timeout, or a CLI we cannot
    /// read — the conservative default.
    #[serde(default)]
    pub quota: Option<Quota>,
}

impl AgentOutput {
    /// Did the CLI exit cleanly with something to say?
    pub fn usable(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0) && !self.text.trim().is_empty()
    }

    /// Did this invocation run out of the CLI's rate limit / quota?
    pub fn quota_exhausted(&self) -> bool {
        self.quota.is_some()
    }
}

/// Invoke `spec` for `seat`, updating the seat's conversation state.
pub async fn invoke(
    spec: &AgentSpec,
    seat: &mut SeatState,
    inv: &Invocation<'_>,
) -> Result<AgentOutput> {
    tokio::fs::create_dir_all(inv.artifacts)
        .await
        .with_context(|| format!("create {}", inv.artifacts.display()))?;
    let prompt_path = inv.artifacts.join(format!("{}.prompt.md", inv.stem));
    tokio::fs::write(&prompt_path, inv.prompt)
        .await
        .with_context(|| format!("write {}", prompt_path.display()))?;

    let plan = build_command(spec, seat, inv, &prompt_path)?;
    tracing::debug!(seat = %seat.key, agent = %spec.id, argv = ?plan.argv, "spawning agent");

    let started = Instant::now();
    let mut cmd = Command::new(&plan.argv[0]);
    cmd.args(&plan.argv[1..])
        .current_dir(inv.cwd)
        .envs(&spec.env)
        .env("MAGI_SEAT", &seat.key)
        .env("MAGI_TURN", seat.turns.to_string())
        .env("MAGI_PROMPT_FILE", &prompt_path)
        .env("MAGI_ALLOW_WRITE", if inv.allow_write { "1" } else { "0" })
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if plan.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn `{}` for seat {}", plan.argv[0], seat.key))?;
    // Feed stdin from a task rather than inline: a `command` agent that never
    // reads its stdin, or a prompt larger than the pipe buffer, would
    // otherwise deadlock here before the process is ever waited on.
    if let (Some(body), Some(mut sink)) = (plan.stdin.clone(), child.stdin.take()) {
        tokio::spawn(async move {
            sink.write_all(body.as_bytes()).await.ok();
            sink.shutdown().await.ok();
        });
    }

    let (stdout, stderr, code, timed_out) =
        match tokio::time::timeout(inv.timeout, child.wait_with_output()).await {
            Ok(res) => {
                let out = res.with_context(|| format!("wait for seat {}", seat.key))?;
                (
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                    out.status.code(),
                    false,
                )
            }
            Err(_) => {
                tracing::warn!(seat = %seat.key, secs = inv.timeout.as_secs(), "agent timed out");
                (String::new(), String::new(), None, true)
            }
        };

    let out_path = inv.artifacts.join(format!("{}.out", inv.stem));
    let err_path = inv.artifacts.join(format!("{}.err", inv.stem));
    tokio::fs::write(&out_path, &stdout).await.ok();
    tokio::fs::write(&err_path, &stderr).await.ok();

    let extracted = extract(spec.kind, &stdout);
    if let Some(session) = extracted.session {
        match spec.kind {
            AgentKind::Claude => seat.claude_session = Some(session),
            AgentKind::Opencode | AgentKind::Antigravity => seat.captured_session = Some(session),
            AgentKind::Command => {}
        }
    }
    if let Some(status) = &extracted.status
        && !status.eq_ignore_ascii_case("success")
    {
        tracing::warn!(seat = %seat.key, status = %status, "agent reported a non-success status");
    }
    let text = if extracted.text.trim().is_empty() {
        // A CLI that printed only to stderr still told us something.
        if stdout.trim().is_empty() {
            stderr.trim().to_owned()
        } else {
            stdout.trim().to_owned()
        }
    } else {
        extracted.text
    };
    seat.turns += 1;

    Ok(AgentOutput {
        text,
        exit_code: code,
        timed_out,
        duration_ms: started.elapsed().as_millis() as u64,
        artifacts: vec![
            file_name(&prompt_path),
            file_name(&out_path),
            file_name(&err_path),
        ],
        quota: extracted.quota,
    })
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// The argv plus optional stdin body for one invocation.
#[derive(Debug)]
struct Plan {
    argv: Vec<String>,
    stdin: Option<String>,
}

/// Text handed to an agent when the prompt is delivered as a file.
fn pointer(prompt_path: &Path) -> String {
    format!(
        "Read the file at {} and follow every instruction in it exactly. That \
         file is your complete task description; this message contains nothing \
         else.",
        prompt_path.display()
    )
}

fn build_command(
    spec: &AgentSpec,
    seat: &SeatState,
    inv: &Invocation<'_>,
    prompt_path: &Path,
) -> Result<Plan> {
    let mut argv: Vec<String> = Vec::new();
    let mut stdin: Option<String> = None;
    let delivery = spec.delivery();
    let resuming = has_session(spec.kind, seat, inv.sessions);

    match spec.kind {
        AgentKind::Claude => {
            argv.push("claude".to_owned());
            argv.push("-p".to_owned());
            argv.push("--output-format".to_owned());
            argv.push("json".to_owned());
            if let Some(m) = &spec.model {
                argv.push("--model".to_owned());
                argv.push(m.clone());
            }
            if inv.sessions {
                let uuid = seat
                    .claude_session
                    .as_deref()
                    .context("claude seat is missing its session uuid")?;
                argv.push(if resuming { "--resume" } else { "--session-id" }.to_owned());
                argv.push(uuid.to_owned());
            }
            argv.push("--permission-mode".to_owned());
            argv.push("bypassPermissions".to_owned());
            if !inv.allow_write {
                argv.push("--disallowed-tools".to_owned());
                argv.push("Edit,Write,MultiEdit,NotebookEdit".to_owned());
            }
        }
        AgentKind::Opencode => {
            argv.push("opencode".to_owned());
            argv.push("run".to_owned());
            argv.push("--format".to_owned());
            argv.push("json".to_owned());
            argv.push("--dir".to_owned());
            argv.push(inv.cwd.to_string_lossy().into_owned());
            // `--auto` gates *every* permission, reads included: without it a
            // non-interactive opencode cannot even open the prompt file, and
            // the seat drops out of the panel with "the user rejected
            // permission to use this specific tool call". opencode has no
            // read-only mode, so read-only seats rely on the prompt plus the
            // fact that judge and reviewer worktrees are disposable — judges'
            // are deleted after the tally, reviewers' are reset to the commit
            // under review every round.
            argv.push("--auto".to_owned());
            if let Some(m) = &spec.model {
                argv.push("-m".to_owned());
                argv.push(m.clone());
            }
            if resuming {
                argv.push("-s".to_owned());
                argv.push(
                    seat.captured_session
                        .clone()
                        .expect("has_session checked the id is present"),
                );
            }
        }
        AgentKind::Antigravity => {
            argv.push("agy".to_owned());
            argv.push("--output-format".to_owned());
            argv.push("json".to_owned());
            // agy's print mode gives up after 5 minutes by default, which is
            // far below an implementation node's budget.
            argv.push("--print-timeout".to_owned());
            argv.push(format!("{}s", inv.timeout.as_secs()));
            argv.push("--mode".to_owned());
            argv.push(
                if inv.allow_write {
                    "accept-edits"
                } else {
                    "plan"
                }
                .to_owned(),
            );
            if inv.allow_write {
                argv.push("--dangerously-skip-permissions".to_owned());
            }
            if let Some(m) = &spec.model {
                argv.push("--model".to_owned());
                argv.push(m.clone());
            }
            if resuming {
                argv.push("--conversation".to_owned());
                argv.push(
                    seat.captured_session
                        .clone()
                        .expect("has_session checked the id is present"),
                );
            }
            // The prompt file lives outside the worktree, so the workspace has
            // to be widened to reach it.
            if delivery == Delivery::File {
                argv.push("--add-dir".to_owned());
                argv.push(inv.artifacts.to_string_lossy().into_owned());
            }
        }
        AgentKind::Command => {
            if spec.command.is_empty() {
                bail!("agent `{}` has kind = \"command\" but no command", spec.id);
            }
            let vars: BTreeMap<&str, String> = BTreeMap::from([
                ("{prompt_file}", prompt_path.to_string_lossy().into_owned()),
                ("{cwd}", inv.cwd.to_string_lossy().into_owned()),
                ("{label}", seat.key.clone()),
                ("{session}", seat.claude_session.clone().unwrap_or_default()),
            ]);
            for raw in &spec.command {
                let mut arg = raw.clone();
                for (k, v) in &vars {
                    if arg.contains(k) {
                        arg = arg.replace(k, v);
                    }
                }
                argv.push(arg);
            }
        }
    }

    argv.extend(spec.extra_args.iter().cloned());

    // `agy` takes the prompt as the value of `-p`, so the flag has to be
    // emitted right before whatever the delivery mode produces.
    if spec.kind == AgentKind::Antigravity {
        argv.push("-p".to_owned());
    }
    match delivery {
        Delivery::Stdin if spec.kind == AgentKind::Antigravity => {
            // agy has no text stdin path; fall back to the pointer file.
            argv.push(pointer(prompt_path));
        }
        Delivery::Stdin => stdin = Some(inv.prompt.to_owned()),
        Delivery::Argv => argv.push(inv.prompt.to_owned()),
        Delivery::File => argv.push(pointer(prompt_path)),
    }

    Ok(Plan { argv, stdin })
}

/// What a CLI's stdout yielded.
#[derive(Debug, Default)]
struct Extracted {
    text: String,
    session: Option<String>,
    status: Option<String>,
    quota: Option<Quota>,
}

/// Pull the agent's message (and any session id) out of a CLI's stdout.
fn extract(kind: AgentKind, stdout: &str) -> Extracted {
    match kind {
        AgentKind::Claude => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
                return Extracted {
                    text: stdout.trim().to_owned(),
                    ..Extracted::default()
                };
            };
            Extracted {
                text: v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                session: v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned),
                status: v.get("is_error").and_then(|e| e.as_bool()).map(|e| {
                    if e {
                        "error".to_owned()
                    } else {
                        "success".to_owned()
                    }
                }),
                quota: claude_quota(&v),
            }
        }
        AgentKind::Opencode => {
            // A JSONL event stream: text parts concatenated in arrival order.
            let mut text = String::new();
            let mut session = None;
            for line in stdout.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                if session.is_none() {
                    session = v
                        .get("sessionID")
                        .and_then(|s| s.as_str())
                        .map(str::to_owned);
                }
                let part = v.get("part").unwrap_or(&serde_json::Value::Null);
                if part.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(t) = part.get("text").and_then(|t| t.as_str())
                {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Extracted {
                text,
                session,
                status: None,
                quota: None,
            }
        }
        AgentKind::Antigravity => {
            // agy prints warnings before the JSON object, so parse the last
            // line that is one rather than the whole stream.
            let obj = stdout
                .lines()
                .rev()
                .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok());
            let Some(v) = obj else {
                return Extracted {
                    text: stdout.trim().to_owned(),
                    ..Extracted::default()
                };
            };
            Extracted {
                text: v
                    .get("response")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                session: v
                    .get("conversation_id")
                    .and_then(|s| s.as_str())
                    .map(str::to_owned),
                status: v.get("status").and_then(|s| s.as_str()).map(str::to_owned),
                quota: None,
            }
        }
        AgentKind::Command => {
            // A `command` agent may wrap a subscription CLI (a fixture, or a
            // thin shim around `claude`). If its output is the claude error
            // shape we recognise the quota the same way, so tests and wrappers
            // do not need their own detection; anything else is just text.
            let quota = serde_json::from_str::<serde_json::Value>(stdout.trim())
                .ok()
                .and_then(|v| claude_quota(&v));
            Extracted {
                text: stdout.trim().to_owned(),
                session: None,
                status: None,
                quota,
            }
        }
    }
}

/// Recognise claude's rate-limit error shape, when it is present.
///
/// The only output we have observed is the JSON object carrying `is_error:
/// true` and a `result` mentioning the session limit. We key on exactly that;
/// every other CLI (and any future shape) returns `None` and is treated as an
/// ordinary failure — the conservative side.
fn claude_quota(v: &serde_json::Value) -> Option<Quota> {
    let is_err = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    if !is_err {
        return None;
    }
    let result = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
    if !result.to_lowercase().contains("session limit") {
        return None;
    }
    // "…session limit · resets 4:50am (Asia/Tokyo)". The timezone read is not
    // worth parsing exactly; keep the whole phrase after "resets" as free text.
    let reset = result
        .split("resets ")
        .nth(1)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(Quota { reset })
}

/// Preflight: which configured agents are not runnable here?
pub fn missing_programs(specs: &[AgentSpec]) -> Vec<String> {
    let mut missing = Vec::new();
    for s in specs {
        let program = match s.kind {
            AgentKind::Command => s.command.first().map(String::as_str),
            other => other.program(),
        };
        if let Some(p) = program
            && !crate::config::which(p)
            && !Path::new(p).is_file()
            && !missing.iter().any(|m: &String| m == p)
        {
            missing.push(p.to_owned());
        }
    }
    missing
}

/// Absolute path of a run's artifact directory.
pub fn artifacts_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("artifacts")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: AgentKind, model: Option<&str>) -> AgentSpec {
        AgentSpec {
            id: "a".to_owned(),
            kind,
            model: model.map(str::to_owned),
            command: vec!["echo".to_owned(), "{label}".to_owned()],
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            prompt_delivery: None,
        }
    }

    fn inv<'a>(cwd: &'a Path, art: &'a Path, allow_write: bool) -> Invocation<'a> {
        Invocation {
            cwd,
            prompt: "do the thing",
            timeout: Duration::from_secs(900),
            allow_write,
            sessions: true,
            artifacts: art,
            stem: "t",
        }
    }

    fn plan_for(kind: AgentKind, seat: &SeatState, allow_write: bool) -> Plan {
        build_command(
            &spec(kind, None),
            seat,
            &inv(Path::new("."), Path::new("/art"), allow_write),
            Path::new("/art/p.md"),
        )
        .unwrap()
    }

    #[test]
    fn claude_mints_then_resumes_the_same_uuid() {
        let mut seat = SeatState::new("judge-1", "a", 7);
        let uuid = seat.claude_session.clone().unwrap();
        let first = plan_for(AgentKind::Claude, &seat, true);
        assert!(first.argv.windows(2).any(|w| w == ["--session-id", &uuid]));
        assert!(!first.argv.iter().any(|a| a == "--resume"));

        seat.turns = 1;
        let second = plan_for(AgentKind::Claude, &seat, true);
        assert!(second.argv.windows(2).any(|w| w == ["--resume", &uuid]));
        assert!(!second.argv.iter().any(|a| a == "--session-id"));
    }

    #[test]
    fn read_only_seats_cannot_edit() {
        let seat = SeatState::new("judge-1", "a", 7);
        let claude = plan_for(AgentKind::Claude, &seat, false);
        assert!(claude.argv.iter().any(|a| a == "--disallowed-tools"));
        assert!(
            !plan_for(AgentKind::Claude, &seat, true)
                .argv
                .iter()
                .any(|a| a == "--disallowed-tools")
        );

        let agy = plan_for(AgentKind::Antigravity, &seat, false);
        assert!(agy.argv.windows(2).any(|w| w == ["--mode", "plan"]));
        assert!(
            !agy.argv
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
        let agy_rw = plan_for(AgentKind::Antigravity, &seat, true);
        assert!(
            agy_rw
                .argv
                .windows(2)
                .any(|w| w == ["--mode", "accept-edits"])
        );
        assert!(
            agy_rw
                .argv
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );

        // opencode is the exception: `--auto` also gates reads, so withholding
        // it silently drops the seat out of the panel. Verified against the CLI
        // — a read-only judge failed with "the user rejected permission to use
        // this specific tool call" while trying to open its own prompt.
        for allow_write in [false, true] {
            assert!(
                plan_for(AgentKind::Opencode, &seat, allow_write)
                    .argv
                    .iter()
                    .any(|a| a == "--auto"),
                "opencode needs --auto even to read (allow_write = {allow_write})"
            );
        }
    }

    #[test]
    fn captured_sessions_resume_only_once_reported() {
        let mut seat = SeatState::new("impl-A", "a", 7);
        seat.turns = 1;
        for kind in [AgentKind::Opencode, AgentKind::Antigravity] {
            assert!(!has_session(kind, &seat, true));
            let p = plan_for(kind, &seat, true);
            assert!(!p.argv.iter().any(|a| a == "-s" || a == "--conversation"));
        }

        seat.captured_session = Some("sid".to_owned());
        assert!(has_session(AgentKind::Opencode, &seat, true));
        assert!(
            plan_for(AgentKind::Opencode, &seat, true)
                .argv
                .windows(2)
                .any(|w| w == ["-s", "sid"])
        );
        assert!(
            plan_for(AgentKind::Antigravity, &seat, true)
                .argv
                .windows(2)
                .any(|w| w == ["--conversation", "sid"])
        );
    }

    #[test]
    fn sessions_disabled_never_resumes() {
        let mut seat = SeatState::new("impl-A", "a", 7);
        seat.turns = 3;
        seat.captured_session = Some("sid".to_owned());
        for kind in [
            AgentKind::Claude,
            AgentKind::Opencode,
            AgentKind::Antigravity,
        ] {
            assert!(!has_session(kind, &seat, false));
        }
    }

    #[test]
    fn long_prompts_never_reach_argv_for_file_delivery_clis() {
        let seat = SeatState::new("judge-1", "a", 7);
        for kind in [AgentKind::Opencode, AgentKind::Antigravity] {
            let p = plan_for(kind, &seat, false);
            assert!(
                p.argv.iter().all(|a| a != "do the thing"),
                "{kind:?} put the prompt on the command line"
            );
            assert!(p.argv.iter().any(|a| a.contains("/art/p.md")));
        }
        // agy has no text stdin, so its `-p` must always carry something.
        let p = plan_for(AgentKind::Antigravity, &seat, false);
        let at = p.argv.iter().position(|a| a == "-p").unwrap();
        assert!(p.argv.get(at + 1).is_some_and(|v| v.contains("p.md")));
        assert!(p.stdin.is_none());
    }

    #[test]
    fn agy_print_timeout_tracks_the_node_budget() {
        let seat = SeatState::new("impl-A", "a", 7);
        let p = build_command(
            &spec(AgentKind::Antigravity, None),
            &seat,
            &Invocation {
                cwd: Path::new("."),
                prompt: "p",
                timeout: Duration::from_secs(3600),
                allow_write: true,
                sessions: true,
                artifacts: Path::new("/art"),
                stem: "t",
            },
            Path::new("/art/p.md"),
        )
        .unwrap();
        assert!(p.argv.windows(2).any(|w| w == ["--print-timeout", "3600s"]));
    }

    #[test]
    fn command_agents_get_placeholders_substituted() {
        let seat = SeatState::new("impl-A", "a", 7);
        let p = plan_for(AgentKind::Command, &seat, true);
        assert_eq!(p.argv[0], "echo");
        assert_eq!(p.argv[1], "impl-A");
        assert_eq!(p.stdin.as_deref(), Some("do the thing"));
    }

    #[test]
    fn claude_rate_limit_is_detected_and_reset_read_when_present() {
        // The exact shape observed in the wild (run 20260831-031005-ae94).
        let stdout = r#"{"is_error": true, "terminal_reason": "api_error",
                        "result": "You've hit your session limit · resets 4:50am (Asia/Tokyo)",
                        "session_id": "b8e928f1-754e-4bd3-86c5-0567763654e3"}"#;
        let out = extract(AgentKind::Claude, stdout);
        let quota = out.quota.as_ref().expect("rate limit must be detected");
        assert_eq!(
            quota.reset.as_deref(),
            Some("4:50am (Asia/Tokyo)"),
            "reset time read from the body"
        );
    }

    #[test]
    fn claude_rate_limit_without_a_readable_reset_is_still_detected() {
        let out = extract(
            AgentKind::Claude,
            r#"{"is_error":true,"result":"session limit reached"}"#,
        );
        let quota = out.quota.expect("rate limit detected without a reset");
        assert!(quota.reset.is_none(), "unknown reset is kept as unknown");
    }

    #[test]
    fn ordinary_failures_are_never_quota() {
        // A normal failed claude call (is_error with a different message).
        let claude_fail = extract(
            AgentKind::Claude,
            r#"{"is_error":true,"result":"account does not exist"}"#,
        );
        assert!(claude_fail.quota.is_none());

        // A command agent that exits 1 with plain text.
        let cmd_fail = extract(AgentKind::Command, "boom");
        assert!(cmd_fail.quota.is_none());

        // A successful call is not quota even if it mentions the phrase.
        let success = extract(
            AgentKind::Command,
            r#"{"is_error":false,"result":"session limit is fine"}"#,
        );
        assert!(success.quota.is_none());
    }

    #[test]
    fn command_agent_can_carry_the_claude_quota_shape() {
        let out = extract(
            AgentKind::Command,
            r#"{"is_error":true,"result":"You've hit your session limit · resets 1:00am (UTC)"}"#,
        );
        assert!(
            out.quota.is_some(),
            "a wrapper emitting the claude shape counts as quota"
        );
    }

    #[test]
    fn claude_json_result_is_extracted() {
        let out = extract(
            AgentKind::Claude,
            r#"{"result":"all done","session_id":"abc","is_error":false}"#,
        );
        assert_eq!(out.text, "all done");
        assert_eq!(out.session.as_deref(), Some("abc"));
        assert_eq!(out.status.as_deref(), Some("success"));
    }

    #[test]
    fn opencode_event_stream_is_concatenated() {
        let stream = concat!(
            r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#,
            "\n",
            r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"first"}}"#,
            "\n",
            "garbage line\n",
            r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"second"}}"#,
            "\n"
        );
        let out = extract(AgentKind::Opencode, stream);
        assert_eq!(out.text, "first\nsecond");
        assert_eq!(out.session.as_deref(), Some("ses_1"));
    }

    #[test]
    fn agy_json_survives_a_leading_warning_line() {
        let stdout = concat!(
            "warning: --mode plan has no effect while slash commands are disabled.\n",
            r#"{"conversation_id":"eaf2d00a","status":"SUCCESS","response":"persimmon\n"}"#,
            "\n"
        );
        let out = extract(AgentKind::Antigravity, stdout);
        assert_eq!(out.text, "persimmon");
        assert_eq!(out.session.as_deref(), Some("eaf2d00a"));
        assert_eq!(out.status.as_deref(), Some("SUCCESS"));
    }

    #[test]
    fn non_json_stdout_falls_back_to_raw_text() {
        let out = extract(AgentKind::Antigravity, "plain answer\n");
        assert_eq!(out.text, "plain answer");
        assert!(out.session.is_none());
    }

    #[tokio::test]
    async fn command_agent_round_trip_writes_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let art = dir.path().join("artifacts");
        let mut seat = SeatState::new("impl-A", "a", 7);
        let mut s = spec(AgentKind::Command, None);
        s.command = vec!["echo".to_owned(), "hello {label}".to_owned()];
        let out = invoke(
            &s,
            &mut seat,
            &Invocation {
                cwd: dir.path(),
                prompt: "unused",
                timeout: Duration::from_secs(30),
                allow_write: true,
                sessions: true,
                artifacts: &art,
                stem: "impl-A",
            },
        )
        .await
        .unwrap();
        assert!(out.usable(), "{out:?}");
        assert!(out.text.contains("hello impl-A"), "{}", out.text);
        assert_eq!(seat.turns, 1);
        assert!(art.join("impl-A.prompt.md").is_file());
        assert!(art.join("impl-A.out").is_file());
    }

    #[tokio::test]
    async fn a_prompt_larger_than_the_pipe_buffer_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let mut seat = SeatState::new("impl-A", "a", 7);
        let mut s = spec(AgentKind::Command, None);
        // `echo` never reads stdin, so an inline write_all would block once the
        // OS pipe buffer filled — long before the process could be waited on.
        s.command = vec!["echo".to_owned(), "done".to_owned()];
        let big = "x".repeat(1_000_000);
        let out = invoke(
            &s,
            &mut seat,
            &Invocation {
                cwd: dir.path(),
                prompt: &big,
                timeout: Duration::from_secs(60),
                allow_write: true,
                sessions: true,
                artifacts: &dir.path().join("artifacts"),
                stem: "big",
            },
        )
        .await
        .unwrap();
        assert!(out.usable(), "{out:?}");
        assert_eq!(out.text, "done");
    }

    #[tokio::test]
    async fn timeout_is_reported_not_hung() {
        let dir = tempfile::tempdir().unwrap();
        let mut seat = SeatState::new("impl-A", "a", 7);
        let mut s = spec(AgentKind::Command, None);
        s.command = vec!["sleep".to_owned(), "30".to_owned()];
        let out = invoke(
            &s,
            &mut seat,
            &Invocation {
                cwd: dir.path(),
                prompt: "unused",
                timeout: Duration::from_millis(300),
                allow_write: true,
                sessions: true,
                artifacts: &dir.path().join("artifacts"),
                stem: "slow",
            },
        )
        .await
        .unwrap();
        assert!(out.timed_out);
        assert!(!out.usable());
    }

    #[test]
    fn missing_programs_reports_command_binaries() {
        let mut s = spec(AgentKind::Command, None);
        s.command = vec!["definitely-not-a-real-binary-xyz".to_owned()];
        assert_eq!(
            missing_programs(&[s]),
            ["definitely-not-a-real-binary-xyz".to_owned()]
        );
    }
}
