//! `magi plan`: the interview that turns an idea into a task file worth
//! competing.
//!
//! A competition is only as good as its task statement. A vague task buys
//! three vague candidates and a coin-toss tally, and the operator finds that
//! out forty minutes and several dollars later. So this module sits in front of
//! [`crate::queue`]: an agent interviews the operator about the idea, writes a
//! full task file, and magi files that file rather than the one-liner the
//! operator would otherwise have typed.
//!
//! # magi does not host the conversation
//!
//! There is no chat loop in here and there must not be one. The operator
//! already has `claude`, `opencode` and `agy`, each with years of work in its
//! own terminal UI - streaming, editing, file pickers, permission prompts. A
//! conversation reimplemented over captured pipes would be worse than all
//! three, and it is not what magi is for.
//!
//! What magi does instead is narrow and mechanical:
//!
//! 1. Write a *briefing* - the idea, the repository, and [`TASK_FILE_SPEC`] -
//!    to a file, telling the agent to interview the operator and write its task
//!    file to a named output path.
//! 2. Spawn the agent with stdin, stdout and stderr **inherited**, so the
//!    operator is talking to that CLI directly, in its own UI, with no magi in
//!    the middle. Nothing is captured and there is no timeout: a human deciding
//!    what to build takes as long as it takes.
//! 3. When the agent exits, read the output path, check it with
//!    [`review_draft`], and file it.
//!
//! # The draft is never thrown away
//!
//! The output path is under [`crate::run::home`]`/drafts` from the start, not a
//! temporary file, and nothing in this module deletes it. A twenty-minute
//! interview that ends in a validation failure must leave the operator holding
//! the draft, named in the error message, so the fix is an edit and
//! `magi task add --file` rather than a second interview. That is the single
//! most important behaviour here, and [`vet`] is the only place that can break
//! it.

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, Result, bail};

use crate::config::{AgentKind, AgentSpec, Config, which};
use crate::queue::{self, Queue, Source, Task};
use crate::run;

/// The task-file shape the leader is asked to produce.
///
/// This is handed to the leader verbatim as part of its briefing, and it is
/// also the document [`review_draft`] enforces. The two are checked against
/// each other by a test, because a spec that asks for something the validator
/// does not require - or worse, the reverse - turns a good interview into a
/// rejected draft for no reason the operator can see.
pub const TASK_FILE_SPEC: &str = "\
The task file is markdown. magi hands it to every candidate verbatim and to
every judge as the statement of what was asked, so it is the only thing any of
them knows about the change. Use this shape:

# <one line, imperative: what the change is>

## Context

Why this change, and what a competent stranger to this repository needs to know
that the code does not say. Name the files, the modules and the symbols
involved, with paths.

## Change

What to do, in enough mechanical detail that two candidates could not
reasonably disagree about the target: the interfaces, the names, the shape of
the data. Leave the *design* open - how it is built, in what order, with what
internal structure. That gap is where blind judging does its work; closing it
turns the competition into three transcriptions of the same answer.

## Constraints

Anything that must hold: files that must not be touched, dependencies that must
not be added, conventions to follow, commands that must not be run.

## Completion criteria

- [ ] One observable, checkable statement per line.
- [ ] Written so that a judge holding only the diff and this list can decide
      whether each line holds. \"Works well\" cannot be judged; \"`magi plan`
      exits non-zero and names the draft path when the draft has no completion
      criteria\" can.

## Out of scope

What this competition must not touch, so that no candidate can win on breadth
instead of on the change that was asked for.

Rules for the task itself:

- One change per competition. Bundling unrelated fixes makes the diff
  unjudgeable and the statistics meaningless.
- Nothing destructive or irreversible. Several candidates run unattended and in
  parallel, and no node stops to ask.
- Visual and UX judgement stays with the operator: no judge sees a rendered
  screen, so do not ask for one to be evaluated.
";

/// Shortest draft magi will treat as a finished task without comment.
///
/// Nothing magic about the number: it is roughly a title plus one criterion,
/// and an interview that produced less than that almost always ended early.
const MIN_DRAFT_BYTES: usize = 200;

/// The problem [`review_draft`] reports for a draft that is merely suspiciously
/// short.
///
/// It is a public constant because it is the *only* problem a caller may
/// override - length alone is a smell, not a defect, and a genuinely small
/// change deserves a small task file. Callers compare against this exact string
/// to separate the warning from the refusals; [`plan`] does, and `magi task
/// add` will when it starts vetting the files it is given.
pub const SHORT_DRAFT: &str = "the draft is under 200 bytes, which is about a \
     title and one criterion: check the interview actually finished";

/// The problem reported for a draft with nothing in it at all.
const EMPTY_DRAFT: &str = "the draft is empty";

/// The problem reported for a draft with no line that could serve as a title.
const NO_TITLE: &str = "no line in the draft can be used as a title: the first \
     non-blank line must say what the change is";

/// The problem reported for a draft with no completion criteria.
const NO_CRITERIA: &str = "no completion criteria: add a `## Completion \
     criteria` heading (or `## 完了条件`) with one checkable statement per line, \
     or the candidates cannot be compared and the judges have nothing to \
     measure against";

/// Headings that mark a completion-criteria section, in the two languages this
/// repository's operator writes tasks in.
const CRITERIA_HEADINGS: [&str; 4] = [
    "completion criteria",
    "acceptance",
    "完了条件",
    "受け入れ基準",
];

/// What `magi plan` was asked to do.
#[derive(Debug, Clone)]
pub struct Opts {
    /// The rough starting idea, if the operator gave one on the command line.
    /// Absent is normal: the interview can start from nothing.
    pub idea: Option<String>,
    /// Repository the task will be competed in.
    pub repo: PathBuf,
    /// Explicit config file, as `--config`.
    pub config: Option<PathBuf>,
    /// Roster agent id to interview with. `None` picks one per the policy in
    /// [`pick`].
    pub agent: Option<String>,
    /// Priority for the filed task.
    pub priority: i32,
    /// File the draft without the confirmation prompt.
    pub yes: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            idea: None,
            repo: PathBuf::from("."),
            config: None,
            agent: None,
            priority: 0,
            yes: false,
        }
    }
}

/// Interview the operator, then file the resulting task.
///
/// Blocks for as long as the conversation lasts, holding the terminal. Returns
/// the task that was filed; the draft it was filed from stays on disk either
/// way.
pub async fn plan(opts: Opts) -> Result<Task> {
    // The whole command is a handover of the terminal to another program's UI.
    // Without one there is nothing to hand over, and the operator would be left
    // watching an agent wait for input that can never arrive.
    if !std::io::stdin().is_terminal() {
        bail!(
            "`magi plan` is an interview and needs a terminal. To file a task \
             without one, pipe it to `magi task add`."
        );
    }

    // Absolute, because the daemon that eventually runs this task has its own
    // working directory and `.` would mean the wrong repository.
    let repo = opts
        .repo
        .canonicalize()
        .unwrap_or_else(|_| opts.repo.clone());
    let (config, _sources) = Config::discover(&repo, opts.config.as_deref())?;
    let leader = pick(&config.agents, opts.agent.as_deref(), &installed)?;

    let dir = drafts_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let id = new_id();
    let draft = dir.join(format!("{id}.md"));
    let brief_path = dir.join(format!("{id}.briefing.md"));
    let brief = briefing(opts.idea.as_deref(), &repo, &draft, &config.graph.language);
    std::fs::write(&brief_path, &brief)
        .with_context(|| format!("write {}", brief_path.display()))?;

    let argv = interactive_argv(&leader, &brief_path, &dir, &repo)?;

    // Say this before handing over the terminal. `opencode` and `agy` are
    // entered plain (see `interactive_argv`), so for those two this line is the
    // operator's only way to know where the briefing is if the agent comes up
    // without having read it.
    println!("leader: {}", leader.display());
    println!("briefing: {}", brief_path.display());
    println!("task file goes to: {}", draft.display());
    println!("talk it through, then let the leader write the task file and exit.\n");

    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(&repo)
        .envs(&leader.env)
        // Inherited, not piped: the operator is talking to this CLI's own UI.
        // Capturing any of the three would replace that UI with magi's, which
        // is the mistake this module exists to avoid.
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // No timeout, and no `kill_on_drop`. Every other agent invocation in magi
    // is bounded because nothing is watching it; this one is bounded by a human
    // who is sitting right there, and killing their conversation on a clock
    // would lose the interview.
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawn {} (is it installed?)", argv[0]))?;
    if !status.success() {
        // Not fatal on its own: an agent that wrote the task file and then
        // exited badly - or that the operator quit with Ctrl-C after it had
        // written - still leaves something worth filing. Whether there is a
        // draft is the question that matters, and `vet` answers it next.
        eprintln!("note: {} exited with {status}", argv[0]);
    }

    let (body, warnings) = vet(&draft)?;
    for w in &warnings {
        eprintln!("warning: {w}");
    }

    let title = queue::title_from(&body, 72);
    if !opts.yes {
        println!("\n{title}");
        println!("draft: {} ({} bytes)", draft.display(), body.len());
        print!("file this task? [y/N] ");
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("read the confirmation")?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            bail!(
                "not filed. The draft is kept at {0} - file it later with \
                 `magi task add --file {0}`.",
                draft.display()
            );
        }
    }

    let q = Queue::open();
    let mut task = Task::new(title, body, repo, Source::Human);
    task.priority = opts.priority;
    q.put(&mut task)?;
    println!("filed {} {}", task.short(), task.title);
    Ok(task)
}

/// Is this draft usable as a magi task?
///
/// Separated from [`plan`] so that the rules are assertable without an
/// interview, and so `magi task add` can reuse them for the files it is handed.
///
/// Every problem found is returned, not just the first: an operator about to
/// edit a draft wants the whole list, and a validator that reveals one defect
/// per run turns one fix into three.
pub fn review_draft(body: &str) -> Result<(), Vec<String>> {
    // An empty draft is reported as exactly one problem. It has no title and no
    // criteria either, but saying so would be three ways of describing the same
    // nothing, and the operator's next action is the same in all three cases.
    if body.trim().is_empty() {
        return Err(vec![EMPTY_DRAFT.to_owned()]);
    }

    let mut problems = Vec::new();

    // Delegated rather than reimplemented: whatever `title_from` would accept
    // is by definition a usable title, since it is what ends up on the task.
    // Its placeholder is the queue's way of saying "there was nothing here".
    if queue::title_from(body, 72) == "(empty task)" {
        problems.push(NO_TITLE.to_owned());
    }

    if !has_completion_criteria(body) {
        problems.push(NO_CRITERIA.to_owned());
    }

    if body.len() < MIN_DRAFT_BYTES {
        problems.push(SHORT_DRAFT.to_owned());
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Read the draft, check it, and split the refusals from the warnings.
///
/// The draft file is read and never written, moved or removed, whatever the
/// outcome - that is what makes a rejected interview recoverable, and the error
/// names the path so the operator does not have to guess it.
fn vet(draft: &Path) -> Result<(String, Vec<String>)> {
    let body = std::fs::read_to_string(draft).with_context(|| {
        format!(
            "no task file at {} - the leader was asked to write one there",
            draft.display()
        )
    })?;
    match review_draft(&body) {
        Ok(()) => Ok((body, Vec::new())),
        Err(problems) => {
            let (soft, hard): (Vec<String>, Vec<String>) =
                problems.into_iter().partition(|p| p == SHORT_DRAFT);
            if hard.is_empty() {
                return Ok((body, soft));
            }
            let list = hard
                .iter()
                .map(|p| format!("  - {p}"))
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "the draft is not usable as a magi task:\n{list}\n\n\
                 It is kept at {0} - nothing was thrown away. Edit it and file \
                 it with `magi task add --file {0}`.",
                draft.display()
            );
        }
    }
}

/// Does this draft state how anyone would know the task was done?
///
/// Two forms count: a heading naming the section, or a checkbox list anywhere.
/// The heading match is deliberately lenient about decoration, because the same
/// section arrives as `## Acceptance`, `**Acceptance criteria**` or `完了条件:`
/// depending on which CLI wrote it, and rejecting a real criteria section over
/// asterisks would teach the operator to distrust the check. An undecorated
/// line has to *be* the phrase, though: prose that happens to contain the word
/// "acceptance" is not a section.
fn has_completion_criteria(body: &str) -> bool {
    body.lines().any(|line| {
        let line = line.trim();
        is_checkbox(line) || is_criteria_heading(line)
    })
}

fn is_criteria_heading(line: &str) -> bool {
    let decorated = line.starts_with(['#', '*', '_']);
    let bare = line
        .trim_start_matches(['#', '*', '_', '>', ' '])
        .trim_end_matches(['#', '*', '_', ':', '：', ' '])
        .trim()
        .to_lowercase();
    CRITERIA_HEADINGS.iter().any(|h| {
        if decorated {
            bare.starts_with(h)
        } else {
            bare == *h
        }
    })
}

fn is_checkbox(line: &str) -> bool {
    let Some(rest) = line.strip_prefix(['-', '*', '+']) else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with("[ ]") || rest.starts_with("[x]") || rest.starts_with("[X]")
}

/// Where drafts live: under the run home, never in a temporary directory the OS
/// may reap and never inside the repository, where it would show up as an
/// untracked file in every candidate's worktree.
fn drafts_dir() -> PathBuf {
    run::home().join("drafts")
}

fn new_id() -> String {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
    let seed = crate::rng::entropy();
    format!("{stamp}-{:04x}", (seed ^ (seed >> 32)) & 0xffff)
}

/// Can this agent's CLI actually be run on this machine?
pub(crate) fn installed(spec: &AgentSpec) -> bool {
    // A `command` agent has no program of its own to look for - its argv is the
    // operator's, and they are the authority on whether it runs.
    spec.kind.program().is_none_or(which)
}

/// Choose the agent that will conduct the interview.
///
/// `available` is a parameter rather than a call to [`which`] so the order
/// below is assertable on a machine with none of these CLIs installed, which is
/// every CI runner.
///
/// The order, and why:
///
/// 1. An explicit `--agent` always wins, and is an error rather than a fallback
///    when it is unusable. The operator naming a leader has a reason, and
///    silently interviewing them with a different model would waste the
///    conversation.
/// 2. Otherwise a [`AgentKind::Claude`] seat, ahead of the roster order. It is
///    the only one of the three CLIs magi can address before the first turn
///    (see [`crate::agent`]'s session table), so it is the only one that can be
///    handed the briefing as an argument and come up already knowing what the
///    interview is for - with the others the operator has to point them at the
///    briefing themselves. For the one command whose whole value is a smooth
///    conversation, that difference decides it.
/// 3. Otherwise the first runnable agent in roster order, because the roster
///    order is the operator's own stated preference and magi has nothing better
///    to go on.
pub(crate) fn pick(
    agents: &[AgentSpec],
    want: Option<&str>,
    available: &dyn Fn(&AgentSpec) -> bool,
) -> Result<AgentSpec> {
    if let Some(id) = want {
        let spec = agents
            .iter()
            .find(|a| a.id == id)
            .with_context(|| format!("no agent `{id}` in the roster; it has {}", ids(agents)))?;
        if !available(spec) {
            bail!(
                "agent `{}` needs `{}` on PATH; install it or pass a different \
                 --agent",
                spec.id,
                spec.kind.program().unwrap_or("its command")
            );
        }
        return Ok(spec.clone());
    }

    if agents.is_empty() {
        bail!(
            "the agent roster is empty, so there is nobody to plan with: \
             install one of claude, opencode or agy - magi derives a roster \
             from what is on PATH - or add an [[agents]] entry to magi.toml."
        );
    }

    if let Some(spec) = agents
        .iter()
        .find(|a| a.kind == AgentKind::Claude && available(a))
    {
        return Ok(spec.clone());
    }

    agents
        .iter()
        .find(|a| available(a))
        .cloned()
        .with_context(|| {
            let missing = agents
                .iter()
                .filter_map(|a| a.kind.program())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "no agent in the roster can be run here: install one of \
                 {missing}, or add an [[agents]] entry to magi.toml for a CLI \
                 you do have"
            )
        })
}

fn ids(agents: &[AgentSpec]) -> String {
    if agents.is_empty() {
        return "no agents at all".to_owned();
    }
    agents
        .iter()
        .map(|a| a.id.clone())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The argv that puts the operator in a conversation with `spec`.
///
/// Deliberately *not* [`crate::agent`]'s `build_command`: that one builds the
/// headless invocation the graph needs - `claude -p`, `opencode run`,
/// `agy --output-format json` - which prints one answer and exits, and would
/// turn this interview into a single non-interactive turn. Every flag there
/// exists to make a CLI machine-readable and unattended; every flag here exists
/// to leave it exactly as interactive as the operator is used to.
///
/// Two other differences from the headless path are on purpose:
///
/// - No permission bypass. `bypassPermissions` / `--dangerously-skip-permissions`
///   are how an unattended node gets work done with nobody to ask. Here there
///   is somebody to ask, sitting at the terminal, and deciding on their behalf
///   would be magi overstepping.
/// - `spec.extra_args` is passed only for `kind = "command"`. For the three
///   known CLIs those arguments were written for the headless invocation - an
///   `--output-format json` or a `--print-timeout` among them ends the
///   interview before it starts. For a `command` agent the whole argv is the
///   operator's, so their arguments *are* the invocation.
fn interactive_argv(
    spec: &AgentSpec,
    brief_path: &Path,
    widen: &Path,
    repo: &Path,
) -> Result<Vec<String>> {
    let mut argv: Vec<String> = Vec::new();
    match spec.kind {
        AgentKind::Claude => {
            argv.push("claude".to_owned());
            if let Some(m) = &spec.model {
                argv.push("--model".to_owned());
                argv.push(m.clone());
            }
            // The briefing and the task file both live under the run home,
            // outside the repository, so the workspace has to be widened to
            // reach them - the same reason `agent::build_command` adds
            // `--add-dir` for a file-delivered prompt.
            argv.push("--add-dir".to_owned());
            argv.push(widen.to_string_lossy().into_owned());
            // The positional argument is claude's opening prompt, and the
            // session stays interactive because `-p` is absent. This is the
            // whole advantage that puts claude first in `pick`.
            argv.push(format!(
                "Read the file at {} and follow it. Interview me about the \
                 change first; write the task file only once I say the plan is \
                 right.",
                brief_path.display()
            ));
        }
        // Entered plain, in the repository. Neither CLI's interactive form has
        // a documented way to be handed an opening prompt that magi can rely
        // on, and guessing a flag would break the one command an operator
        // cannot work around by editing a config file. They read the briefing
        // because magi printed its path before handing over the terminal.
        AgentKind::Opencode => argv.push("opencode".to_owned()),
        AgentKind::Antigravity => {
            argv.push("agy".to_owned());
            argv.push("--add-dir".to_owned());
            argv.push(widen.to_string_lossy().into_owned());
        }
        AgentKind::Command => {
            if spec.command.is_empty() {
                bail!("agent `{}` has kind = \"command\" but no command", spec.id);
            }
            // Same placeholders as the headless path, so an operator's existing
            // `command` agent works here without a second spelling to learn.
            for raw in &spec.command {
                argv.push(
                    raw.replace("{prompt_file}", &brief_path.to_string_lossy())
                        .replace("{cwd}", &repo.to_string_lossy()),
                );
            }
            argv.extend(spec.extra_args.iter().cloned());
        }
    }
    Ok(argv)
}

/// What the leader is told before it starts talking.
fn briefing(idea: Option<&str>, repo: &Path, out: &Path, language: &str) -> String {
    let idea = match idea.map(str::trim).filter(|s| !s.is_empty()) {
        Some(i) => i.to_owned(),
        None => "The operator has not written the idea down yet. Ask them what \
                 they want to change, starting from the repository itself."
            .to_owned(),
    };
    // The interview is the operator talking, so their language matters more
    // here than it does in any prompt the graph sends: an agent that answers a
    // Japanese question in English makes the conversation slower for exactly
    // the person magi is trying to help.
    let lang = if language.trim().is_empty() || language.eq_ignore_ascii_case("en") {
        String::new()
    } else {
        format!("\n\nConduct the interview in {language}, and write the task file in {language}.")
    };
    format!(
        "You are the planning leader for magi, which runs a blind \
         multi-agent implementation competition: several agents will implement \
         the task file you write, in isolated worktrees, unaware of each other, \
         and judges will rank the results without knowing who wrote what.\n\n\
         Your job is not to implement anything. It is to interview the operator \
         until the change is pinned down, and then write one task file.\n\n\
         # Repository\n\n{repo}\n\n\
         Read it before you start asking. Questions that the code already \
         answers spend the operator's patience for nothing.\n\n\
         # The idea\n\n{idea}\n\n\
         # How to run the interview\n\n\
         - Ask about what you cannot determine yourself: intent, scope, which \
         of several defensible designs the operator wants, what must not \
         change.\n\
         - Ask a few questions at a time and wait for the answers. Do not \
         produce the task file after one exchange.\n\
         - Disagree when you have grounds. A leader that agrees with everything \
         adds nothing to what the operator already typed.\n\
         - Confirm the plan in your own words and get an explicit yes before \
         writing.\n\n\
         # What to write, and where\n\n\
         When the operator agrees the plan is right, write the task file to \
         exactly this path:\n\n{out}\n\n\
         Write that file and nothing else. Do not modify the repository: the \
         competing agents do the implementation, and a repository you have \
         already edited makes their diffs unjudgeable.\n\n\
         magi will refuse a task file with no completion criteria, so those are \
         not optional.\n\n\
         # Task file specification\n\n{spec}\n\n\
         When the file is written, tell the operator it is done and exit.{lang}",
        repo = repo.display(),
        out = out.display(),
        spec = TASK_FILE_SPEC,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task file of the shape `AGENTS.md` and [`TASK_FILE_SPEC`] describe.
    fn good_draft() -> String {
        "# Report per-node durations in `magi show`\n\
         \n\
         ## Context\n\
         \n\
         `report::run` prints a run's nodes but not how long each took, so the \
         numbers behind a slow competition have to be recovered from \
         `run.json`'s `events` with `jq`.\n\
         \n\
         ## Change\n\
         \n\
         Add a duration column to the node table in `src/report.rs`, computed \
         from the existing `events` timestamps in `RunState`.\n\
         \n\
         ## Constraints\n\
         \n\
         No new dependencies. Do not change `run.json`'s schema.\n\
         \n\
         ## Completion criteria\n\
         \n\
         - [ ] `magi show <id>` prints a duration for every finished node.\n\
         - [ ] A node still running prints its elapsed time, not a blank.\n\
         - [ ] `cargo test` passes.\n\
         \n\
         ## Out of scope\n\
         \n\
         The TUI's detail pane.\n"
            .to_owned()
    }

    fn spec(id: &str, kind: AgentKind) -> AgentSpec {
        AgentSpec {
            id: id.to_owned(),
            kind,
            model: None,
            command: Vec::new(),
            extra_args: Vec::new(),
            env: Default::default(),
            prompt_delivery: None,
        }
    }

    /// Availability stub: an agent is runnable unless its id was listed as
    /// missing. Keeps the selection tests off `PATH` entirely.
    fn without<'a>(missing: &'a [&'a str]) -> impl Fn(&AgentSpec) -> bool + 'a {
        move |a: &AgentSpec| !missing.contains(&a.id.as_str())
    }

    #[test]
    fn a_realistic_task_file_is_accepted() {
        let draft = good_draft();
        assert!(
            draft.len() >= MIN_DRAFT_BYTES,
            "the fixture must be a real task file, not a stub"
        );
        assert_eq!(review_draft(&draft), Ok(()));
    }

    #[test]
    fn a_bad_draft_reports_every_problem_at_once_rather_than_one_per_run() {
        // Markdown decoration and nothing else: no usable title, no criteria,
        // and far too short.
        let problems = review_draft("###\n\n- - -\n").expect_err("must be rejected");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(problems[0], NO_TITLE);
        assert_eq!(problems[1], NO_CRITERIA);
        assert_eq!(problems[2], SHORT_DRAFT);
    }

    #[test]
    fn an_empty_draft_is_reported_as_empty_and_not_as_three_other_things() {
        for body in ["", "   \n\t\n  "] {
            let problems = review_draft(body).expect_err("must be rejected");
            assert_eq!(problems, vec![EMPTY_DRAFT.to_owned()], "body {body:?}");
        }
    }

    #[test]
    fn a_draft_without_a_usable_title_is_rejected() {
        // Long enough, and it has criteria - the title is the only defect.
        let body = format!(
            "#\n\n## Completion criteria\n\n- it works\n\n{}",
            "x".repeat(300)
        );
        assert_eq!(
            review_draft(&body).expect_err("must be rejected"),
            vec![NO_TITLE.to_owned()]
        );
    }

    #[test]
    fn a_draft_without_completion_criteria_is_rejected_on_that_alone() {
        let body = format!(
            "# Rework the config loader\n\n## Change\n\nMake it layered.\n\n{}",
            "prose. ".repeat(60)
        );
        assert!(body.len() >= MIN_DRAFT_BYTES);
        assert_eq!(
            review_draft(&body).expect_err("must be rejected"),
            vec![NO_CRITERIA.to_owned()]
        );
    }

    #[test]
    fn completion_criteria_are_recognised_in_english_and_japanese_and_as_checkboxes() {
        let filler = "x".repeat(300);
        for section in [
            "## Completion criteria\n\n- everything holds",
            "## Acceptance\n\n- everything holds",
            "### Acceptance criteria (all of them)\n\n- everything holds",
            "**Completion criteria**\n\n- everything holds",
            "## 完了条件\n\n- 全部そろっている",
            "## 受け入れ基準\n\n- 全部そろっている",
            "完了条件:\n\n- 全部そろっている",
            "- [ ] no heading at all, just a checkbox",
        ] {
            let body = format!("# A real change\n\n{section}\n\n{filler}");
            assert_eq!(
                review_draft(&body),
                Ok(()),
                "must accept criteria written as {section:?}"
            );
        }
    }

    #[test]
    fn prose_that_merely_mentions_acceptance_is_not_a_criteria_section() {
        let body = format!(
            "# A real change\n\nAcceptance of the design is up to you.\n\n{}",
            "x".repeat(300)
        );
        assert_eq!(
            review_draft(&body).expect_err("prose is not a section"),
            vec![NO_CRITERIA.to_owned()]
        );
    }

    #[test]
    fn a_complete_but_tiny_draft_is_warned_about_and_not_refused() {
        let body = "# Bump the poll interval to 5s\n\n## Completion criteria\n\n- [ ] it is 5s\n";
        assert!(body.len() < MIN_DRAFT_BYTES);
        let problems = review_draft(body).expect_err("must warn");
        assert_eq!(problems, vec![SHORT_DRAFT.to_owned()]);

        // And `vet` must let it through as a warning rather than a refusal,
        // which is what makes `--yes` able to override length alone.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.md");
        std::fs::write(&path, body).unwrap();
        let (read_back, warnings) = vet(&path).expect("length alone must not refuse");
        assert_eq!(read_back, body);
        assert_eq!(warnings, vec![SHORT_DRAFT.to_owned()]);
    }

    /// The behaviour a twenty-minute interview depends on: a draft magi refuses
    /// is still there, byte for byte, at the path the refusal prints.
    #[test]
    fn a_refused_draft_is_still_on_disk_at_the_path_the_error_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("20260902-231501-ab12.md");
        let body = "# Something the operator spent twenty minutes on\n\nBut with no criteria.\n";
        std::fs::write(&path, body).unwrap();

        let err = vet(&path).expect_err("no criteria must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains(&path.display().to_string()),
            "the error must name the draft path: {msg}"
        );
        assert!(msg.contains("magi task add --file"), "{msg}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("the draft must survive its refusal"),
            body
        );
    }

    #[test]
    fn a_draft_lives_under_the_run_home_so_it_outlives_the_command_that_wrote_it() {
        assert_eq!(drafts_dir(), run::home().join("drafts"));
    }

    #[test]
    fn a_missing_draft_is_reported_against_the_path_the_leader_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-written.md");
        let msg = vet(&path).expect_err("nothing to file").to_string();
        assert!(msg.contains(&path.display().to_string()), "{msg}");
    }

    #[test]
    fn the_leader_is_the_claude_seat_even_when_it_is_not_first_in_the_roster() {
        let agents = [
            spec("oc", AgentKind::Opencode),
            spec("opus", AgentKind::Claude),
            spec("agy", AgentKind::Antigravity),
        ];
        let got = pick(&agents, None, &without(&[])).expect("a leader");
        assert_eq!(got.id, "opus");
    }

    #[test]
    fn the_leader_falls_back_to_the_first_installed_agent_in_roster_order() {
        let agents = [
            spec("opus", AgentKind::Claude),
            spec("oc", AgentKind::Opencode),
            spec("agy", AgentKind::Antigravity),
        ];
        let got = pick(&agents, None, &without(&["opus", "oc"])).expect("a leader");
        assert_eq!(got.id, "agy");
    }

    #[test]
    fn an_empty_roster_says_what_to_install() {
        let msg = pick(&[], None, &without(&[]))
            .expect_err("nobody to plan with")
            .to_string();
        assert!(msg.contains("roster is empty"), "{msg}");
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("magi.toml"), "{msg}");
    }

    #[test]
    fn a_roster_with_nothing_installed_names_the_programs_that_are_missing() {
        let agents = [
            spec("opus", AgentKind::Claude),
            spec("oc", AgentKind::Opencode),
        ];
        let err = pick(&agents, None, &without(&["opus", "oc"])).expect_err("nothing runnable");
        let msg = format!("{err:#}");
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("opencode"), "{msg}");
    }

    #[test]
    fn an_explicitly_named_agent_wins_over_the_claude_preference() {
        let agents = [
            spec("opus", AgentKind::Claude),
            spec("oc", AgentKind::Opencode),
        ];
        let got = pick(&agents, Some("oc"), &without(&[])).expect("a leader");
        assert_eq!(got.id, "oc");
    }

    #[test]
    fn an_unknown_agent_id_lists_the_ids_that_do_exist() {
        let agents = [
            spec("opus", AgentKind::Claude),
            spec("oc", AgentKind::Opencode),
        ];
        let msg = pick(&agents, Some("gemini"), &without(&[]))
            .expect_err("no such agent")
            .to_string();
        assert!(msg.contains("gemini"), "{msg}");
        assert!(msg.contains("opus, oc"), "{msg}");
    }

    #[test]
    fn an_explicitly_named_agent_that_is_not_installed_is_an_error_not_a_fallback() {
        let agents = [
            spec("opus", AgentKind::Claude),
            spec("oc", AgentKind::Opencode),
        ];
        let msg = pick(&agents, Some("oc"), &without(&["oc"]))
            .expect_err("must not silently interview with another model")
            .to_string();
        assert!(msg.contains("opencode"), "{msg}");
        assert!(msg.contains("--agent"), "{msg}");
    }

    /// The spec and the validator are one contract in two places, and only this
    /// test keeps them from drifting: a spec that stopped asking for completion
    /// criteria would produce drafts magi refuses, with the operator following
    /// magi's own instructions.
    #[test]
    fn the_task_file_spec_asks_for_the_completion_criteria_the_validator_requires() {
        assert!(TASK_FILE_SPEC.contains("## Completion criteria"));
        assert!(has_completion_criteria(TASK_FILE_SPEC));
        assert_eq!(
            review_draft(TASK_FILE_SPEC),
            Ok(()),
            "the spec must pass the validator it is paired with"
        );
    }

    #[test]
    fn the_briefing_carries_the_idea_the_repository_the_output_path_and_the_spec() {
        let b = briefing(
            Some("make the queue drain faster"),
            Path::new("/src/magi"),
            Path::new("/home/magi/drafts/x.md"),
            "en",
        );
        assert!(b.contains("make the queue drain faster"));
        assert!(b.contains("/src/magi"));
        assert!(b.contains("/home/magi/drafts/x.md"));
        assert!(b.contains("## Completion criteria"));
        assert!(
            !b.contains("Conduct the interview in"),
            "en adds no language line"
        );
    }

    #[test]
    fn a_briefing_without_an_idea_tells_the_leader_to_start_the_conversation() {
        let b = briefing(
            Some("   "),
            Path::new("/src/magi"),
            Path::new("/o.md"),
            "ja",
        );
        assert!(b.contains("has not written the idea down yet"));
        assert!(b.contains("Conduct the interview in ja"));
    }

    #[test]
    fn the_interactive_invocation_is_never_the_headless_one() {
        let brief = Path::new("/home/magi/drafts/x.briefing.md");
        let widen = Path::new("/home/magi/drafts");
        let repo = Path::new("/src/magi");

        let mut claude = spec("opus", AgentKind::Claude);
        claude.model = Some("opus".to_owned());
        // Flags that would end the conversation, and the ones that carry it.
        let argv = interactive_argv(&claude, brief, widen, repo).unwrap();
        assert_eq!(argv[0], "claude");
        assert!(!argv.iter().any(|a| a == "-p" || a == "--output-format"));
        assert!(!argv.iter().any(|a| a == "--permission-mode"));
        assert!(argv.windows(2).any(|w| w == ["--model", "opus"]));
        assert!(
            argv.windows(2)
                .any(|w| w == ["--add-dir", "/home/magi/drafts"])
        );
        assert!(
            argv.last().unwrap().contains(&brief.display().to_string()),
            "claude gets the briefing as its opening prompt: {argv:?}"
        );

        assert_eq!(
            interactive_argv(&spec("oc", AgentKind::Opencode), brief, widen, repo).unwrap(),
            vec!["opencode".to_owned()],
            "opencode is entered plain, in the repository"
        );
        assert_eq!(
            interactive_argv(&spec("agy", AgentKind::Antigravity), brief, widen, repo).unwrap(),
            vec![
                "agy".to_owned(),
                "--add-dir".to_owned(),
                "/home/magi/drafts".to_owned()
            ]
        );
    }

    #[test]
    fn a_command_agent_gets_its_own_argv_with_the_briefing_substituted_in() {
        let mut cmd = spec("local", AgentKind::Command);
        cmd.command = vec![
            "my-agent".to_owned(),
            "--brief".to_owned(),
            "{prompt_file}".to_owned(),
            "--in".to_owned(),
            "{cwd}".to_owned(),
        ];
        cmd.extra_args = vec!["--interactive".to_owned()];
        let argv = interactive_argv(
            &cmd,
            Path::new("/b.md"),
            Path::new("/drafts"),
            Path::new("/src/magi"),
        )
        .unwrap();
        assert_eq!(
            argv,
            vec![
                "my-agent",
                "--brief",
                "/b.md",
                "--in",
                "/src/magi",
                "--interactive"
            ]
        );

        let empty = spec("broken", AgentKind::Command);
        let msg = interactive_argv(&empty, Path::new("/b.md"), Path::new("/d"), Path::new("/r"))
            .expect_err("a command agent with no command cannot be spawned")
            .to_string();
        assert!(msg.contains("broken"), "{msg}");
    }
}
