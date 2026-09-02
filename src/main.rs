//! `magi` command line entry point.
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{ArgAction, CommandFactory as _, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use magi::config::{Config, MergeMode};
use magi::graph::{Runner, fold_run};
use magi::queue::{self, Queue, Source, Task, TaskStatus};
use magi::run::{RunState, latest_id, list_ids, resolve_id};
use magi::{agent, daemon, report, stats, tui, updater, web};

/// Blind multi-agent implementation competition.
#[derive(Debug, Parser)]
#[command(name = "magi", version, about, long_about = None)]
struct Cli {
    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,
    /// Disable colour (also respected via NO_COLOR).
    #[arg(long, global = true)]
    no_color: bool,
    /// Omitted: open the TUI on a terminal, print the latest run otherwise.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Merge mode, on the command line.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum MergeArg {
    /// Print the merge command and stop.
    None,
    /// `git merge --no-ff` into the base branch.
    Local,
    /// Push and open a pull request with `gh`.
    Pr,
}

impl From<MergeArg> for MergeMode {
    fn from(a: MergeArg) -> Self {
        match a {
            MergeArg::None => Self::None,
            MergeArg::Local => Self::Local,
            MergeArg::Pr => Self::Pr,
        }
    }
}

impl MergeArg {
    /// Name as the config parser spells it. Written out rather than derived
    /// from `Debug`, which would change silently if a variant were renamed.
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Local => "local",
            Self::Pr => "pr",
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a competition for one task.
    Run {
        /// The task. Omit when using --file, --issue, or --resume.
        instruction: Vec<String>,
        /// Repository to work on.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Config file; defaults to <repo>/magi.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Read the task from a file.
        #[arg(long, conflicts_with = "instruction")]
        file: Option<PathBuf>,
        /// Read the task from a GitHub issue via `gh`.
        #[arg(long, conflicts_with_all = ["instruction", "file"])]
        issue: Option<u64>,
        /// Continue an interrupted run.
        #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["instruction", "file", "issue"])]
        resume: Option<String>,
        /// Parallel implementations.
        #[arg(short = 'c', long)]
        candidates: Option<usize>,
        /// Independent judges.
        #[arg(short = 'j', long)]
        judges: Option<usize>,
        /// Review + fix rounds before giving up.
        #[arg(long)]
        review_rounds: Option<usize>,
        /// What to do with the winning branch.
        #[arg(long, value_enum)]
        merge: Option<MergeArg>,
        /// Seed for label assignment, to reproduce a run.
        #[arg(long)]
        seed: Option<u64>,
        /// Prepare and print the plan without spending an agent call.
        #[arg(long)]
        dry_run: bool,
    },
    /// Run only the review + verification + gate loop, on work that already
    /// exists on a branch. Nothing competes: no implementation, no judging, no
    /// vote. This is the cheap half of the graph, for hand-written changes.
    Review {
        /// Branch holding the work to review.
        branch: String,
        /// Repository to work on.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Config file; defaults to <repo>/magi.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Reviewers per round.
        #[arg(long)]
        reviewers: Option<usize>,
        /// Review + fix rounds before giving up.
        #[arg(long)]
        review_rounds: Option<usize>,
        /// What to do with the branch once it is clean.
        #[arg(long, value_enum)]
        merge: Option<MergeArg>,
    },
    /// List recorded runs.
    List {
        /// How many to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show one run in full. Defaults to the most recent.
    Show {
        /// Run id or unambiguous prefix/suffix.
        id: Option<String>,
        /// Print the raw state file instead.
        #[arg(long)]
        json: bool,
    },
    /// Open the observation deck: every run, live, in one screen. This is what
    /// bare `magi` does on a terminal.
    Tui,
    /// Aggregate win rates, reviewer precision, and verification yield.
    Stats,
    /// Remove a run's worktrees and branches.
    Fold {
        /// Run id; defaults to the most recent.
        id: Option<String>,
        /// Also drop the winner's worktree and branch.
        #[arg(long)]
        all: bool,
    },
    /// Read, file, and hold work in the queue that `magi serve` drains.
    ///
    /// This is the surface an agent uses too: an implementer that spots
    /// something worth doing but out of scope runs `magi task add`, and the
    /// task is attributed to its run rather than to a passing human.
    Task {
        /// What to do with the queue.
        #[command(subcommand)]
        command: TaskCmd,
    },
    /// Drain the queue unattended: take the next task, run the graph, repeat.
    ///
    /// One competition at a time on purpose. The graph is already parallel
    /// inside (candidates times judges), and two at once doubles the burn on
    /// the agent-CLI quota that is the real constraint.
    Serve {
        /// Repository used by tasks that name none.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Config file; defaults to <repo>/magi.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Seconds between queue polls.
        #[arg(long, default_value_t = 5)]
        poll: u64,
        /// Attempts a task gets before it is held for a human.
        #[arg(long, default_value_t = 2)]
        max_attempts: usize,
        /// Drain what is runnable now, then stop.
        #[arg(long)]
        once: bool,
        /// What to do with each winning branch.
        #[arg(long, value_enum)]
        merge: Option<MergeArg>,
    },
    /// Serve the phone UI: the same runs and queue, from a browser.
    ///
    /// There is no authentication. The default bind is the machine's Tailscale
    /// address precisely so the tailnet is the boundary; it is not a mistake
    /// that this does not listen on 0.0.0.0.
    Web {
        /// `auto` for the Tailscale address, or an explicit IP.
        #[arg(long, default_value_t = web::Bind::Auto)]
        bind: web::Bind,
        /// Port to listen on.
        #[arg(long, default_value_t = web::DEFAULT_PORT)]
        port: u16,
        /// Repository used by tasks filed without one.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Check the environment and the resolved roster.
    Doctor {
        /// Repository to inspect.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Config file; defaults to <repo>/magi.toml.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Write a starter magi.toml.
    Init {
        /// Where to write it.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Print a shell completion script.
    Completion {
        /// Target shell.
        shell: Shell,
    },
    /// Update the magi binary from GitHub releases.
    SelfUpdate {
        /// Only report whether an update exists.
        #[arg(long)]
        check_only: bool,
        /// Install without asking.
        #[arg(long)]
        yes: bool,
    },
}

/// Operations on the queue.
#[derive(Debug, Subcommand)]
enum TaskCmd {
    /// File a task. Text as arguments, or --file, or --issue, or on stdin.
    Add {
        /// The task text.
        #[arg(value_name = "TASK", trailing_var_arg = true)]
        instruction: Vec<String>,
        /// Read the task from a file.
        #[arg(long, conflicts_with_all = ["instruction", "issue"])]
        file: Option<PathBuf>,
        /// Read the task from a GitHub issue via `gh`.
        #[arg(long, conflicts_with_all = ["instruction", "file"])]
        issue: Option<u64>,
        /// One-line summary. Defaults to the first meaningful line.
        #[arg(long)]
        title: Option<String>,
        /// Higher runs first.
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        priority: i32,
        /// Repository the task applies to.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Print the filed task as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the queue.
    List {
        /// Include finished and held tasks.
        #[arg(long)]
        all: bool,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show one task in full.
    Show {
        /// Task id or unambiguous prefix/suffix.
        id: String,
        /// Print the raw task file instead.
        #[arg(long)]
        json: bool,
    },
    /// Take a task out of the loop's reach, keeping it on disk.
    Hold {
        /// Task id or unambiguous prefix/suffix.
        id: String,
    },
    /// Put a held or finished task back in line, attempts reset.
    Release {
        /// Task id or unambiguous prefix/suffix.
        id: String,
    },
    /// Delete a task.
    Rm {
        /// Task id or unambiguous prefix/suffix.
        id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let interactive = std::io::stdout().is_terminal();
    if cli.no_color || std::env::var_os("NO_COLOR").is_some() || !interactive {
        report::set_color(false);
    }

    // Bare `magi` opens the observation deck — but only on a terminal. Piped or
    // in CI it must not raise an alternate screen and block on input, so it
    // degrades to the report the pipe was almost certainly after.
    let command = cli.command.unwrap_or(if interactive {
        Command::Tui
    } else {
        Command::Show {
            id: None,
            json: false,
        }
    });

    // Overlap the release check with the command: a run spends minutes waiting
    // on agent latency, so this is free, and it is drained with a bounded wait
    // so a slow network cannot delay the exit.
    let pending = spawn_update_check(&command);
    let result = dispatch(command).await;
    updater::finalize(pending, std::time::Duration::from_millis(1500)).await;
    result
}

/// Start the background release check, unless this command is about updating,
/// printing static text, or holding the whole screen.
fn spawn_update_check(command: &Command) -> Option<updater::Pending> {
    if matches!(
        command,
        Command::SelfUpdate { .. } | Command::Completion { .. } | Command::Tui
    ) {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    // A broken config must not stop the command, let alone the update check.
    let cfg = Config::discover(&cwd, None)
        .map(|(c, _)| c)
        .unwrap_or_default();
    updater::spawn(&cfg.update, &tokio::runtime::Handle::current())
}

async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Run {
            instruction,
            repo,
            config,
            file,
            issue,
            resume,
            candidates,
            judges,
            review_rounds,
            merge,
            seed,
            dry_run,
        } => {
            let mut runner = if let Some(id) = resume {
                let runner = Runner::resume(&id)?;
                println!(
                    "{}",
                    format_args!("resuming {} ({:?})", runner.state.id, runner.state.status)
                );
                runner
            } else {
                let task = task_text(&instruction, file.as_deref(), issue).await?;
                let (mut cfg, from) = Config::discover(&repo, config.as_deref())?;
                if let Some(n) = candidates {
                    cfg.graph.candidates = n;
                }
                if let Some(n) = judges {
                    cfg.graph.judges = n;
                }
                if let Some(n) = review_rounds {
                    cfg.graph.review_rounds = n;
                }
                if let Some(m) = merge {
                    cfg.merge.mode = m.into();
                }
                if let Some(s) = seed {
                    cfg.blind.seed = Some(s);
                }
                println!("config: {}", describe_layers(&from));
                Runner::start(&repo, task, cfg).await?
            };

            if dry_run {
                print!("{}", report::run(&runner.state));
                println!("\ndry run: stopping before the first agent call");
                return Ok(());
            }

            let result = runner.execute().await;
            print!("{}", report::run(&runner.state));
            result
        }

        Command::Review {
            branch,
            repo,
            config,
            reviewers,
            review_rounds,
            merge,
        } => {
            let (mut cfg, from) = Config::discover(&repo, config.as_deref())?;
            if let Some(n) = reviewers {
                cfg.graph.reviewers = n;
            }
            if let Some(n) = review_rounds {
                cfg.graph.review_rounds = n;
            }
            if let Some(m) = merge {
                cfg.merge.mode = m.into();
            }
            println!("config: {}", describe_layers(&from));
            let mut runner = Runner::review(&repo, &branch, cfg).await?;
            let result = runner.execute().await;
            print!("{}", report::run(&runner.state));
            result
        }

        Command::List { limit } => {
            let ids = list_ids();
            if ids.is_empty() {
                println!("no runs yet");
                return Ok(());
            }
            for id in ids.into_iter().take(limit) {
                match RunState::load(&id) {
                    Ok(s) => println!("{}", report::line(&s)),
                    Err(e) => println!("{id}  <unreadable: {e}>"),
                }
            }
            Ok(())
        }

        Command::Show { id, json } => {
            let id = match id {
                Some(i) => resolve_id(&i)?,
                None => latest_id().context("no runs yet")?,
            };
            if json {
                let path = magi::run::run_dir(&id).join("run.json");
                print!(
                    "{}",
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("read {}", path.display()))?
                );
            } else {
                print!("{}", report::run(&RunState::load(&id)?));
            }
            Ok(())
        }

        // Colour is already decided in `main`: bare `magi` only reaches here on
        // a terminal, and `--no-color` / NO_COLOR turned it off there. The
        // report pane parses those same ANSI codes back into ratatui spans.
        Command::Tui => tui::run(),

        Command::Stats => {
            let states = stats::load_all();
            print!("{}", report::stats(&stats::collect(&states)));
            Ok(())
        }

        Command::Fold { id, all } => {
            let id = match id {
                Some(i) => resolve_id(&i)?,
                None => latest_id().context("no runs yet")?,
            };
            let mut state = RunState::load(&id)?;
            let removed = fold_run(&mut state, all).await?;
            if removed.is_empty() {
                println!("{id}: nothing left to fold");
            } else {
                for r in removed {
                    println!("removed {r}");
                }
            }
            Ok(())
        }

        Command::Serve {
            repo,
            config,
            poll,
            max_attempts,
            once,
            merge,
        } => {
            daemon::serve(daemon::Opts {
                repo,
                config,
                poll: std::time::Duration::from_secs(poll),
                max_attempts,
                once,
                merge: merge.map(|m| m.as_str().to_owned()),
            })
            .await
        }

        Command::Web { bind, port, repo } => {
            web::serve(web::Opts {
                bind,
                port,
                repo,
                open: false,
            })
            .await
        }

        Command::Task { command } => task_cmd(command).await,

        Command::Doctor { repo, config } => doctor(&repo, config.as_deref()).await,

        Command::Init { repo, force } => {
            let path = repo.join("magi.toml");
            if path.exists() && !force {
                bail!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                );
            }
            std::fs::write(&path, Config::starter_toml())
                .with_context(|| format!("write {}", path.display()))?;
            println!("wrote {}", path.display());
            Ok(())
        }

        Command::Completion { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_owned();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(())
        }

        Command::SelfUpdate { check_only, yes } => {
            updater::run_self_update(yes, check_only, !std::io::stdin().is_terminal()).await
        }
    }
}

/// Human-readable list of the config layers in effect.
fn describe_layers(layers: &[PathBuf]) -> String {
    if layers.is_empty() {
        return "built-in defaults (run `magi init`)".to_owned();
    }
    layers
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" < ")
}

fn init_logging(verbose: u8) {
    let default = match verbose {
        0 => "magi=info",
        1 => "magi=debug",
        _ => "magi=trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("MAGI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}

/// Resolve the task text from argv, a file, or a GitHub issue.
async fn task_text(words: &[String], file: Option<&Path>, issue: Option<u64>) -> Result<String> {
    if let Some(path) = file {
        let body =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if body.trim().is_empty() {
            bail!("{} is empty", path.display());
        }
        return Ok(body);
    }
    if let Some(number) = issue {
        let out = tokio::process::Command::new("gh")
            .args([
                "issue",
                "view",
                &number.to_string(),
                "--json",
                "title,body",
                "--template",
                "{{.title}}\n\n{{.body}}",
            ])
            .output()
            .await
            .context("spawn gh (is the GitHub CLI installed?)")?;
        if !out.status.success() {
            bail!(
                "gh issue view {number}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let body = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if body.is_empty() {
            bail!("issue #{number} has no title or body");
        }
        return Ok(format!("Resolve GitHub issue #{number}.\n\n{body}"));
    }
    let joined = words.join(" ");
    if !joined.trim().is_empty() {
        return Ok(joined);
    }
    // Stdin, when it is not a terminal. An agent filing a task has a body, not
    // a tidy argv, and quoting a multi-paragraph markdown task through a shell
    // is how task text gets mangled. On a terminal we must not do this: it
    // would hang waiting for input the operator has no reason to expect.
    if !std::io::stdin().is_terminal() {
        use std::io::Read as _;
        let mut body = String::new();
        std::io::stdin()
            .read_to_string(&mut body)
            .context("read the task from stdin")?;
        if !body.trim().is_empty() {
            return Ok(body);
        }
    }
    bail!("give a task: as arguments, --file, --issue, or on stdin");
}

/// The `magi task` verbs.
async fn task_cmd(command: TaskCmd) -> Result<()> {
    let q = Queue::open();
    match command {
        TaskCmd::Add {
            instruction,
            file,
            issue,
            title,
            priority,
            repo,
            json,
        } => {
            let text = task_text(&instruction, file.as_deref(), issue).await?;
            let title = title.unwrap_or_else(|| queue::title_from(&text, 72));
            let source = task_source(issue).await;
            // Store an absolute path: the daemon that runs this task has its
            // own working directory, and `.` would mean the wrong repository.
            let repo = repo.canonicalize().unwrap_or(repo);
            let mut task = Task::new(title, text, repo, source);
            task.priority = priority;
            q.put(&mut task)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&task)?);
            } else {
                println!(
                    "filed {} [{}] {}",
                    task.short(),
                    task.source.label(),
                    task.title
                );
            }
            Ok(())
        }

        TaskCmd::List { all, json } => {
            let tasks: Vec<Task> = q
                .list()
                .into_iter()
                .filter(|t| all || t.status != TaskStatus::Done)
                .collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
                return Ok(());
            }
            if tasks.is_empty() {
                println!("queue empty");
                return Ok(());
            }
            for t in &tasks {
                let attempts = if t.attempts > 0 {
                    format!(" x{}", t.attempts)
                } else {
                    String::new()
                };
                println!(
                    "{}  {:<9}{:<4} {:<14} {}",
                    t.short(),
                    t.status.as_str(),
                    attempts,
                    t.source.label(),
                    t.title
                );
            }
            Ok(())
        }

        TaskCmd::Show { id, json } => {
            let t = q.get(&id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&t)?);
                return Ok(());
            }
            println!("{}  {}", t.id, t.title);
            println!("status    {}", t.status.as_str());
            println!("source    {}", t.source.label());
            println!("repo      {}", t.repo.display());
            println!("priority  {}", t.priority);
            println!("attempts  {}", t.attempts);
            if !t.runs.is_empty() {
                println!("runs      {}", t.runs.join(", "));
            }
            if let Some(e) = &t.last_error {
                println!("last      {e}");
            }
            println!("\n{}", t.instruction.trim_end());
            Ok(())
        }

        TaskCmd::Hold { id } => {
            let mut t = q.get(&id)?;
            t.hold();
            q.put(&mut t)?;
            println!("held {} {}", t.short(), t.title);
            Ok(())
        }

        TaskCmd::Release { id } => {
            let mut t = q.get(&id)?;
            t.release();
            q.put(&mut t)?;
            println!("queued {} {}", t.short(), t.title);
            Ok(())
        }

        TaskCmd::Rm { id } => {
            let removed = q.remove(&id)?;
            println!("removed {removed}");
            Ok(())
        }
    }
}

/// Who is filing this task.
///
/// An agent inside a run is identified by the environment the graph gave it, so
/// no flag can be forgotten or forged by accident: `MAGI_RUN` is set only by
/// `agent::invoke`. That is what makes "86% of tasks were filed by agents" a
/// measurement rather than a claim.
async fn task_source(issue: Option<u64>) -> Source {
    if let Some(number) = issue {
        let repo = tokio::process::Command::new("gh")
            .args([
                "repo",
                "view",
                "--json",
                "nameWithOwner",
                "-q",
                ".nameWithOwner",
            ])
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".to_owned());
        return Source::Issue { number, repo };
    }
    match std::env::var("MAGI_RUN") {
        Ok(run) if !run.trim().is_empty() => Source::Agent {
            run,
            node: std::env::var("MAGI_NODE").unwrap_or_else(|_| "agent".to_owned()),
        },
        _ => Source::Human,
    }
}

async fn doctor(repo: &Path, config: Option<&Path>) -> Result<()> {
    println!("git        {}", probe("git", &["--version"]).await);
    println!("gh         {}", probe("gh", &["--version"]).await);
    for kind in ["claude", "opencode", "agy"] {
        println!(
            "{kind:<10} {}",
            if magi::config::which(kind) {
                "found".to_owned()
            } else {
                "not on PATH".to_owned()
            }
        );
    }

    let toplevel = magi::git::toplevel(repo).await;
    match &toplevel {
        Ok(p) => println!("\nrepo       {}", p.display()),
        Err(e) => println!("\nrepo       not a git repository: {e}"),
    }
    if let Ok(p) = &toplevel {
        println!(
            "clean      {}",
            match magi::git::is_clean(p).await {
                Ok(true) => "yes".to_owned(),
                Ok(false) => "no — magi refuses to start on a dirty tree".to_owned(),
                Err(e) => format!("unknown: {e}"),
            }
        );
    }

    let (cfg, from) = Config::discover(repo, config)?;
    println!("config     {}", describe_layers(&from));
    let missing = agent::missing_programs(&cfg.agents);
    if !missing.is_empty() {
        println!("missing    {}", missing.join(", "));
    }
    match cfg.resolve_roles() {
        Ok(roles) => {
            println!("\nroster");
            for a in &cfg.agents {
                println!("  {}", a.display());
            }
            println!("\nseats");
            for (i, a) in roles.implementers.iter().enumerate() {
                println!("  implement {}  {}", i + 1, a.display());
            }
            for (i, a) in roles.judges.iter().enumerate() {
                println!("  judge     {}  {}", i + 1, a.display());
            }
            for (i, a) in roles.reviewers.iter().enumerate() {
                println!("  review    {}  {}", i + 1, a.display());
            }
            println!(
                "  fix          {}",
                roles
                    .fixer
                    .as_ref()
                    .map_or("the winner's own author".to_owned(), |f| f.display())
            );
        }
        Err(e) => println!("\nroster     unusable: {e}"),
    }
    println!(
        "\nverify.e2e   {}\nverify.gate  {}\nmerge        {:?}",
        if cfg.verify.e2e.is_empty() {
            "(none — the review loop has no real-machine leg)".to_owned()
        } else {
            cfg.verify.e2e.join(" && ")
        },
        if cfg.verify.gate.is_empty() {
            "(none — nothing blocks a merge)".to_owned()
        } else {
            cfg.verify.gate.join(" && ")
        },
        cfg.merge.mode
    );
    println!("runs         {}", magi::run::runs_root().display());
    Ok(())
}

async fn probe(program: &str, args: &[&str]) -> String {
    match tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("(no output)")
            .to_owned(),
        Err(e) => format!("not available: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[tokio::test]
    async fn task_from_argv_is_joined() {
        let words = vec!["add".to_owned(), "retries".to_owned()];
        assert_eq!(task_text(&words, None, None).await.unwrap(), "add retries");
    }

    #[tokio::test]
    async fn empty_task_is_rejected() {
        assert!(task_text(&[], None, None).await.is_err());
        assert!(task_text(&["   ".to_owned()], None, None).await.is_err());
    }

    #[tokio::test]
    async fn task_from_file_is_read_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.md");
        std::fs::write(&path, "line one\nline two\n").unwrap();
        let text = task_text(&[], Some(&path), None).await.unwrap();
        assert!(text.contains("line two"));

        let empty = dir.path().join("empty.md");
        std::fs::write(&empty, "  \n").unwrap();
        assert!(task_text(&[], Some(&empty), None).await.is_err());
    }
}
