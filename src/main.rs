//! `magi` command line entry point.
use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use clap::{ArgAction, CommandFactory as _, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
use magi::config::{Config, MergeMode};
use magi::graph::{Runner, fold_run};
use magi::queue::{self, Queue, Source, Task, TaskStatus};
use magi::run::{RunState, latest_id, list_ids, resolve_id};
use magi::{agent, ask, daemon, plan, report, repos, stats, tui, updater, web};

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

/// Execution knobs a competition run takes, shared by `magi run <instruction>`
/// and the free-text fallback of `magi run rm <words...>` (see [`RunCmd::Rm`]):
/// once clap commits to the `rm` subcommand it stops recognising `Run`'s own
/// flags, so a flag placed after prose that happens to start with "rm"
/// (`magi run rm the dead code --candidates 3`) needs its own declaration or
/// parsing fails outright. Left unset (`None`/default) on whichever side of
/// "rm" the caller did not use.
#[derive(Debug, Clone, Default, clap::Args)]
struct RunOpts {
    /// Repository to work on.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// Config file; defaults to <repo>/magi.toml.
    #[arg(long)]
    config: Option<PathBuf>,
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
}

impl RunOpts {
    /// Combines the flags clap captured on each side of a literal "rm" that
    /// turned out to be prose. Only one side is ever populated in practice —
    /// a flag is either before "rm" or after it — so preferring `self` is an
    /// arbitrary but harmless tie-break for the case of a flag on both sides.
    fn merge(self, other: Self) -> Self {
        Self {
            repo: self.repo.or(other.repo),
            config: self.config.or(other.config),
            candidates: self.candidates.or(other.candidates),
            judges: self.judges.or(other.judges),
            review_rounds: self.review_rounds.or(other.review_rounds),
            merge: self.merge.or(other.merge),
            seed: self.seed.or(other.seed),
            dry_run: self.dry_run || other.dry_run,
        }
    }
}

/// Operations on recorded runs.
#[derive(Debug, Subcommand)]
enum RunCmd {
    /// Delete a recorded run.
    ///
    /// Takes every remaining word, not just one: clap commits to this
    /// subcommand as soon as it sees the literal token "rm", so an
    /// instruction that happens to start with "rm" (`magi run rm the dead
    /// code`) must still parse here rather than erroring out. Dispatch tells
    /// the two apart by word count — exactly one word is a run id, more than
    /// one is free-text instruction that starts with "rm".
    Rm {
        /// Run id or unambiguous prefix/suffix.
        #[arg(required = true)]
        id: Vec<String>,
        #[command(flatten)]
        opts: Box<RunOpts>,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a competition for one task.
    Run {
        #[command(subcommand)]
        command: Option<RunCmd>,
        /// The task. Omit when using --file, --issue, or --resume.
        instruction: Vec<String>,
        /// Read the task from a file.
        #[arg(long, conflicts_with = "instruction")]
        file: Option<PathBuf>,
        /// Read the task from a GitHub issue via `gh`.
        #[arg(long, conflicts_with_all = ["instruction", "file"])]
        issue: Option<u64>,
        /// Continue an interrupted run.
        #[arg(long, value_name = "RUN_ID", conflicts_with_all = ["instruction", "file", "issue"])]
        resume: Option<String>,
        #[command(flatten)]
        opts: RunOpts,
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
        /// What the loop this server runs should do with each winning branch.
        ///
        /// `magi web` runs the loop itself now, so the override `magi serve`
        /// accepts has to be expressible here too - otherwise starting the
        /// loop from a phone would mean accepting whatever the repository's
        /// config says, and the terminal would still be required to change it.
        #[arg(long, value_enum)]
        merge: Option<MergeArg>,
    },
    /// Talk over an idea with a leader agent, then file the task it writes.
    ///
    /// magi hands your terminal to the agent's own interface for the interview
    /// and takes it back to validate and queue the result. It does not
    /// reimplement a chat window.
    Plan {
        /// A rough starting idea. Omit to start from nothing.
        #[arg(value_name = "IDEA", trailing_var_arg = true)]
        idea: Vec<String>,
        /// Repository the task will be competed in: a path, or a short
        /// `owner/repo` name resolved against `[repos] roots`.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Config file; defaults to <repo>/magi.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Roster agent id to interview with.
        #[arg(long)]
        agent: Option<String>,
        /// Higher runs first.
        #[arg(long, default_value_t = 0, allow_negative_numbers = true)]
        priority: i32,
        /// File the draft without the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// A browser-interview chat id (or unambiguous prefix/suffix) to
        /// continue here: its transcript and repository go into this
        /// interview's opening briefing as background.
        #[arg(long)]
        from: Option<String>,
    },
    /// List local repositories found under `[repos] roots`.
    Repos {
        /// Re-scan rather than trust anything cached. The CLI keeps no cache
        /// between invocations, so this only matters for symmetry with
        /// `GET /api/repos?refresh=1` - every `magi repos` already scans
        /// fresh.
        #[arg(long)]
        refresh: bool,
    },
    /// Ask the owner something and wait. Meant for agents inside a run.
    Ask {
        /// One-line question.
        #[arg(long)]
        summary: String,
        /// Longer explanation, markdown. Reads stdin when omitted.
        #[arg(long)]
        detail: Option<String>,
        /// An answer to offer; repeat for more. Omit for a free-text reply.
        #[arg(long = "choice")]
        choices: Vec<String>,
        /// Seconds to wait. Defaults to the config's answer_timeout.
        #[arg(long)]
        timeout: Option<u64>,
        /// An HTML page to show with the question: a diff, a table, images.
        ///
        /// Rendered in a sandbox with no JavaScript and no network access, so
        /// inline the CSS and reference assets by bare filename.
        #[arg(long)]
        panel: Option<PathBuf>,
        /// A file the panel references, copied in beside it; repeat for more.
        #[arg(long = "asset", requires = "panel")]
        assets: Vec<PathBuf>,
        /// Repository, for the config that supplies the notify command.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Answer a question an agent is waiting on.
    Answer {
        /// Question id or unambiguous prefix/suffix. Omit for the oldest open one.
        id: Option<String>,
        /// The answer: one of the offered choices, or free text.
        #[arg(long, conflicts_with = "list")]
        reply: Option<String>,
        /// Show the open questions and stop.
        #[arg(long)]
        list: bool,
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
    /// Mark a task finished, without running anything.
    ///
    /// For work that landed by a route the loop did not see: merged by hand,
    /// or a run whose merge succeeded while the run recorded a failure. The
    /// alternative was `release`, which puts the task back in line and pays
    /// for the whole competition again to redo something already in `main`.
    Done {
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

/// Every word clap accepts as a subcommand, at any depth, including aliases.
///
/// Read off the parser rather than listed by hand: a hand-written copy would
/// be one refactor away from disagreeing with the command it is meant to
/// describe, and a guard that silently stops guarding is worse than none.
fn subcommand_words() -> std::collections::BTreeSet<String> {
    fn walk(cmd: &clap::Command, into: &mut std::collections::BTreeSet<String>) {
        for sub in cmd.get_subcommands() {
            into.insert(sub.get_name().to_owned());
            for alias in sub.get_all_aliases() {
                into.insert(alias.to_owned());
            }
            walk(sub, into);
        }
    }
    let mut words = std::collections::BTreeSet::new();
    walk(&<Cli as clap::CommandFactory>::command(), &mut words);
    words
}

/// Refuse an instruction that is really a mistyped command, and say how to
/// insist.
///
/// `magi run show 3cbf` reads like a query and is not one: there is no `run
/// show`, so every word becomes the task, and magi opens worktrees and starts
/// paying agents to implement the sentence "show 3cbf". That happened twice in
/// one morning - once to an agent verifying its own change to this very
/// argument parsing, which is how a run nobody asked for came to exist, and
/// once to the operator.
///
/// The rule is deliberately blunt: a first word that names any subcommand is a
/// typo unless the caller wrote `--`. It costs a legitimate instruction like
/// "add retries to the client" one extra token, and it costs a mistyped
/// command nothing at all instead of a full competition.
fn mistyped_command(
    instruction: &[String],
    separator: bool,
    words: &std::collections::BTreeSet<String>,
) -> Option<String> {
    if separator {
        return None;
    }
    let first = instruction.first()?;
    if !words.contains(first.as_str()) {
        return None;
    }
    Some(format!(
        "`{first}` names a magi subcommand, so `magi run {}` looks like a \
         mistyped command rather than a task, and starting a competition for \
         it would cost real agent calls. Write `magi run -- {}` to mean it \
         literally.",
        instruction.join(" "),
        instruction.join(" ")
    ))
}

/// Whether the caller wrote a literal `--` separator.
///
/// clap consumes it and `ArgMatches` cannot be asked about it afterwards, so
/// the raw argv is the only place the answer still exists.
fn had_separator() -> bool {
    std::env::args_os().any(|arg| arg == "--")
}

async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Run {
            command,
            instruction,
            file,
            issue,
            resume,
            opts,
        } => {
            // A single trailing word is a run id: `magi run rm <id>`. More
            // than one means clap only grabbed "rm" because it matches the
            // subcommand name — this is really an instruction that starts
            // with "rm" (`magi run rm the dead code in auth.rs`), so put the
            // word back and fall through to the normal instruction path,
            // taking along whatever flags clap parsed after "rm" merged with
            // whatever it parsed before.
            let (instruction, opts) = match command {
                Some(RunCmd::Rm { id, .. }) if id.len() == 1 => {
                    return run_rm_cmd(&id[0]);
                }
                Some(RunCmd::Rm { id, opts: rm_opts }) => {
                    let mut full = vec!["rm".to_owned()];
                    full.extend(id);
                    (full, rm_opts.merge(opts))
                }
                None => (instruction, opts),
            };
            if let Some(why) = mistyped_command(&instruction, had_separator(), &subcommand_words())
            {
                bail!(why);
            }
            let repo = opts.repo.unwrap_or_else(|| PathBuf::from("."));
            let mut runner = if let Some(id) = resume {
                let runner = Runner::resume(&id)?;
                println!(
                    "{}",
                    format_args!("resuming {} ({:?})", runner.state.id, runner.state.status)
                );
                runner
            } else {
                let task = task_text(&instruction, file.as_deref(), issue).await?;
                let (mut cfg, from) = Config::discover(&repo, opts.config.as_deref())?;
                if let Some(n) = opts.candidates {
                    cfg.graph.candidates = n;
                }
                if let Some(n) = opts.judges {
                    cfg.graph.judges = n;
                }
                if let Some(n) = opts.review_rounds {
                    cfg.graph.review_rounds = n;
                }
                if let Some(m) = opts.merge {
                    cfg.merge.mode = m.into();
                }
                if let Some(s) = opts.seed {
                    cfg.blind.seed = Some(s);
                }
                println!("config: {}", describe_layers(&from));
                Runner::start(&repo, task, cfg).await?
            };

            if opts.dry_run {
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

        Command::Web {
            bind,
            port,
            repo,
            merge,
        } => {
            web::serve(web::Opts {
                bind,
                port,
                repo,
                open: false,
                merge: merge.map(|m| m.as_str().to_owned()),
            })
            .await
        }

        Command::Plan {
            idea,
            repo,
            config,
            agent,
            priority,
            yes,
            from,
        } => {
            let idea = idea.join(" ");
            let task = plan::plan(plan::Opts {
                idea: (!idea.trim().is_empty()).then_some(idea),
                repo,
                config,
                agent,
                priority,
                yes,
                from,
            })
            .await?;
            println!("filed {} {}", task.short(), task.title);
            Ok(())
        }

        Command::Repos { refresh: _ } => repos_cmd(),

        Command::Ask {
            summary,
            detail,
            choices,
            timeout,
            panel,
            assets,
            repo,
        } => {
            ask_cmd(AskArgs {
                summary,
                detail,
                choices,
                timeout,
                panel,
                assets,
                repo,
            })
            .await
        }

        Command::Answer { id, reply, list } => answer_cmd(id, reply, list),

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

/// `magi ask`: file a question and block until the owner answers.
///
/// This is the command an agent runs, so its exit status carries the outcome:
/// zero with the answer on stdout, non-zero when nobody answered in time. An
/// agent that cannot tell "the owner said Redis" from "the owner never came
/// back" would happily implement a guess.
/// Everything `magi ask` was given, kept together because clap's arms and this
/// function would otherwise drift apart one argument at a time.
struct AskArgs {
    summary: String,
    detail: Option<String>,
    choices: Vec<String>,
    timeout: Option<u64>,
    panel: Option<PathBuf>,
    assets: Vec<PathBuf>,
    repo: PathBuf,
}

async fn ask_cmd(args: AskArgs) -> Result<()> {
    let AskArgs {
        summary,
        detail,
        choices,
        timeout,
        panel,
        assets,
        repo,
    } = args;
    let detail = match detail {
        Some(d) => d,
        // Long explanations arrive on stdin for the same reason task bodies do:
        // quoting markdown through a shell is how it gets mangled.
        None if !std::io::stdin().is_terminal() => {
            use std::io::Read as _;
            let mut body = String::new();
            std::io::stdin()
                .read_to_string(&mut body)
                .context("read the question detail from stdin")?;
            body
        }
        None => String::new(),
    };

    // The run and seat come from the environment the graph set, so a question
    // is attributed to the seat that asked it rather than to whoever is at the
    // terminal. Outside a run those are empty and the question still works.
    let run = std::env::var("MAGI_RUN").unwrap_or_default();
    let node = std::env::var("MAGI_NODE").unwrap_or_else(|_| "ask".to_owned());
    let seat = std::env::var("MAGI_SEAT").unwrap_or_else(|_| "operator".to_owned());

    let (cfg, _) = Config::discover(&repo, None).unwrap_or_default();
    let wait = std::time::Duration::from_secs(timeout.unwrap_or(cfg.graph.answer_timeout));

    let store = ask::Questions::open();
    let mut q = ask::Question::new(run, node, seat, summary, detail, choices);
    // The panel is attached before the question is filed: a question that
    // appears on the phone a moment before its evidence does is a question the
    // owner answers without the evidence.
    if let Some(path) = &panel {
        let html =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        store.put_panel(&mut q, &html, &assets)?;
    }
    store.put(&mut q)?;
    eprintln!("asked {} — waiting for the owner", q.short());

    match ask::ask_and_wait(&mut q, &store, &cfg.notify, wait).await? {
        Some(answer) => {
            println!("{answer}");
            Ok(())
        }
        None => bail!(
            "question {} went unanswered for {}s; it is recorded as abandoned",
            q.short(),
            wait.as_secs()
        ),
    }
}

/// `magi answer`: reply from the terminal, so the phone is a convenience and
/// never the only way to unblock a run.
fn answer_cmd(id: Option<String>, reply: Option<String>, list: bool) -> Result<()> {
    let store = ask::Questions::open();
    let open: Vec<ask::Question> = store
        .list()
        .into_iter()
        .filter(|q| q.status.open())
        .collect();

    if list || (id.is_none() && reply.is_none()) {
        if open.is_empty() {
            println!("nothing is waiting on you");
            return Ok(());
        }
        for q in &open {
            println!("{}  {}", q.short(), q.summary);
            if q.free_text() {
                println!("      free text");
            } else {
                println!("      {}", q.choices.join(" | "));
            }
        }
        return Ok(());
    }

    let mut q = match id {
        Some(id) => store.get(&id)?,
        // Oldest first: the question that has been blocking longest.
        None => open
            .into_iter()
            .next_back()
            .context("nothing is waiting on you")?,
    };
    let reply = reply.context("give the answer with --reply")?;
    let answer = if q.free_text() {
        ask::Answer::Text(reply)
    } else {
        ask::Answer::Choice(reply)
    };
    q.answer(answer)?;
    store.put(&mut q)?;
    println!("answered {} {}", q.short(), q.summary);
    Ok(())
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

        TaskCmd::Done { id } => {
            let mut t = q.get(&id)?;
            t.succeed();
            q.put(&mut t)?;
            println!("done {} {}", t.short(), t.title);
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
            let resolved = q.resolve_id(&id)?;
            let in_flight = magi::daemon::is_working_on_task(
                &magi::run::home(),
                &resolved,
                jiff::Timestamp::now(),
            );
            let removed = q.remove(&resolved, in_flight)?;
            println!("removed {removed}");
            Ok(())
        }
    }
}

/// `magi repos`: everything [`repos::scan`] finds under `[repos] roots`, one
/// per line as `owner/repo` and its path.
///
/// Reads config against the current directory - `[repos] roots` is a machine
/// fact, most often declared in the machine config layer, so which repository
/// this command happens to run inside rarely matters. No cache: a one-shot
/// process has nothing to keep one in, so every invocation already scans
/// fresh - `--refresh` on this command is accepted for symmetry with
/// `GET /api/repos?refresh=1` and changes nothing.
fn repos_cmd() -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (cfg, _) = Config::discover(&cwd, None)?;
    print!("{}", repos_text(&repos::scan(&cfg.repos.roots)));
    Ok(())
}

/// The text `magi repos` prints: one `owner/repo` and its path per line.
///
/// Separated from [`repos_cmd`] so the format is assertable without a real
/// `[repos] roots` scan - the same split [`doctor_queue_and_loop`] uses for
/// the same reason.
fn repos_text(found: &[repos::Repo]) -> String {
    if found.is_empty() {
        return "no repositories found under [repos] roots\n".to_owned();
    }
    let mut s = String::new();
    for r in found {
        let _ = writeln!(s, "{}  {}", r.name, r.path.display());
    }
    s
}

/// Delete a recorded run directory.
fn run_rm_cmd(id: &str) -> Result<()> {
    let resolved = resolve_id(id)?;
    let state = RunState::load(&resolved)?;
    let in_flight =
        magi::daemon::is_working_on(&magi::run::home(), &resolved, jiff::Timestamp::now());
    state.ensure_can_delete(in_flight)?;
    let dir = magi::run::run_dir(&resolved);
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("remove run directory {}", dir.display()))?;
    // The agent that asked died with the run, so an open question would keep
    // asking for a decision nobody can deliver.
    let abandoned = ask::Questions::open().abandon_for_run(
        &resolved,
        &format!("run {resolved} was deleted, so nothing is waiting for this answer"),
    )?;
    println!("removed {resolved}");
    if abandoned > 0 {
        println!(
            "abandoned {abandoned} open question{}",
            if abandoned == 1 { "" } else { "s" }
        );
    }
    Ok(())
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
                // Not a blocker: a run branches off the base branch, so the
                // working copy's state cannot reach it either way.
                Ok(false) => "no — uncommitted work is not part of a run".to_owned(),
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
            // The interview seat, resolved the same way `plan` and the browser
            // conversation resolve it. Shown because a setting an operator
            // cannot confirm is a setting they have to take on faith.
            println!(
                "  plan         {}",
                match magi::plan::pick(
                    &cfg.agents,
                    cfg.roles.planner.as_deref(),
                    &magi::plan::installed,
                ) {
                    Ok(s) => s.display(),
                    Err(e) => format!("unusable: {e}"),
                }
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
    print!("{}", doctor_queue_and_loop(&magi::run::home()));
    Ok(())
}

/// The queue and loop section of `magi doctor`: how much work is backed up,
/// whether `magi serve` is the one moving it, and how far this build's view
/// of the runs directory can be trusted.
///
/// Separate from the async probes above and driven from an explicit `home`
/// rather than the process-global [`magi::run::home`], so a test can point it
/// at a temp directory instead of fighting the `OnceLock` every other caller
/// of that function shares. A fresh install has no queue directory and no
/// daemon file; both collapse to empty results rather than an error, so
/// `doctor` stays the first thing worth running on one.
fn doctor_queue_and_loop(home: &Path) -> String {
    let mut s = String::new();

    let tasks = Queue::at(home.join("queue")).list();
    let (mut queued, mut running, mut failed, mut held, mut done) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    for t in &tasks {
        match t.status {
            TaskStatus::Queued => queued += 1,
            TaskStatus::Running => running += 1,
            TaskStatus::Failed => failed += 1,
            TaskStatus::Held => held += 1,
            TaskStatus::Done => done += 1,
        }
    }
    let _ = writeln!(
        s,
        "\nqueue      {}",
        if tasks.is_empty() {
            "empty".to_owned()
        } else {
            format!("queued {queued}, running {running}, failed {failed}, held {held}, done {done}")
        }
    );
    if held > 0 {
        // The one queue state nothing will move without a human, so it gets
        // its own line and the command that fixes it, rather than being
        // just one more number in the summary above.
        let _ = writeln!(
            s,
            "held       {held} task{} waiting on a human — release with `magi task release`",
            if held == 1 { "" } else { "s" }
        );
    }

    // Reuse daemon.rs's own staleness rule rather than re-deriving it: a
    // heartbeat this build calls fresh must never disagree with what the web
    // UI or the daemon's own log already said about the same file.
    let now = jiff::Timestamp::now();
    let status = daemon::read_status(home);
    let alive = status.as_ref().is_some_and(|s| s.running(now));

    if running > 0 && !alive {
        // Worse than held, and easier to miss: nothing will ever settle these.
        // The loop only offers itself runnable tasks, and `running` is not one
        // of them, so a task whose daemon was killed mid-competition sits
        // there forever while the summary above cheerfully counts it as work
        // in progress.
        let _ = writeln!(
            s,
            "orphaned   {running} task{} left running by a daemon that is gone — \
             `magi task release` to try again, `magi task rm` to drop",
            if running == 1 { "" } else { "s" }
        );
    }

    match &status {
        Some(status) if status.running(now) => {
            match status.pid {
                Some(pid) => {
                    let _ = writeln!(s, "loop       running (pid {pid})");
                }
                None => {
                    let _ = writeln!(s, "loop       running");
                }
            }
            if let Some(current) = &status.current {
                let _ = writeln!(s, "  working  task {} (run {})", current.task, current.run);
            }
        }
        Some(_) => {
            let _ = writeln!(s, "loop       not running (last heartbeat is stale)");
        }
        None => {
            let _ = writeln!(s, "loop       not running");
        }
    }

    // Label kept inside the 11-column gutter the rest of `doctor` uses; at
    // "runs unreadable" the value hung four characters past every other row.
    let _ = writeln!(s, "unreadable {}", web::runs_unreadable(&home.join("runs")));

    s
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
    use magi::run::RunStatus;

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

    #[test]
    fn doctor_reports_an_empty_queue_and_no_loop_on_a_fresh_install() {
        let dir = tempfile::tempdir().unwrap();
        let text = doctor_queue_and_loop(dir.path());
        assert!(text.contains("queue      empty"), "{text}");
        assert!(!text.contains("held"), "nothing to release: {text}");
        assert!(text.contains("loop       not running"), "{text}");
        assert!(text.contains("unreadable 0"), "{text}");
    }

    fn task(status: TaskStatus) -> Task {
        let mut t = Task::new(
            "add retries".to_owned(),
            "add retries".to_owned(),
            PathBuf::from("/repo"),
            Source::Human,
        );
        t.status = status;
        t
    }

    #[test]
    fn repos_text_says_nothing_found_rather_than_printing_an_empty_table() {
        let text = repos_text(&[]);
        assert!(text.contains("no repositories found"), "{text}");
    }

    #[test]
    fn repos_text_prints_one_line_of_name_and_path_per_repository() {
        let found = [
            repos::Repo {
                name: "yukimemi/magi".to_owned(),
                path: PathBuf::from("/home/yukimemi/src/github.com/yukimemi/magi"),
            },
            repos::Repo {
                name: "yukimemi/rvpm".to_owned(),
                path: PathBuf::from("/home/yukimemi/src/github.com/yukimemi/rvpm"),
            },
        ];
        let text = repos_text(&found);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[0].contains("yukimemi/magi")
                && lines[0].contains("/home/yukimemi/src/github.com/yukimemi/magi"),
            "{text}"
        );
        assert!(
            lines[1].contains("yukimemi/rvpm")
                && lines[1].contains("/home/yukimemi/src/github.com/yukimemi/rvpm"),
            "{text}"
        );
    }

    #[test]
    fn doctor_counts_queued_tasks_and_calls_out_a_held_one_with_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Queue::at(dir.path().join("queue"));
        for status in [TaskStatus::Queued, TaskStatus::Queued, TaskStatus::Held] {
            queue.put(&mut task(status)).unwrap();
        }

        let text = doctor_queue_and_loop(dir.path());

        assert!(
            text.contains("queue      queued 2, running 0, failed 0, held 1, done 0"),
            "{text}"
        );
        assert!(
            text.contains(
                "held       1 task waiting on a human — release with `magi task release`"
            ),
            "{text}"
        );
        assert!(text.contains("loop       not running"), "{text}");
    }

    #[test]
    fn a_task_left_running_by_a_dead_daemon_is_called_out() {
        let dir = tempfile::tempdir().unwrap();
        let q = Queue::at(dir.path().join("queue"));
        let mut t = magi::queue::Task::new(
            "port the retry logic".to_owned(),
            "do it".to_owned(),
            PathBuf::from("/repo"),
            magi::queue::Source::Human,
        );
        t.start("20260903-080619-01c2".to_owned());
        q.put(&mut t).unwrap();

        // No daemon at all: the task can only be sitting there.
        let orphaned = doctor_queue_and_loop(dir.path());
        assert!(
            orphaned.contains("orphaned   1 task left running"),
            "{orphaned}"
        );
        assert!(
            orphaned.contains("magi task release"),
            "the report must name the way out: {orphaned}"
        );

        // A live daemon working on it is ordinary progress, not an orphan.
        let mut beat = daemon::Status::new();
        beat.current = Some(daemon::Current {
            task: t.id.clone(),
            run: "20260903-080619-01c2".to_owned(),
        });
        beat.updated_at = jiff::Timestamp::now();
        daemon::write_status_to(&dir.path().join("daemon.json"), &beat).unwrap();
        let working = doctor_queue_and_loop(dir.path());
        assert!(!working.contains("orphaned"), "{working}");
    }

    #[test]
    fn doctor_reports_the_loop_running_a_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut status = magi::daemon::Status::new();
        status.pid = 4242;
        status.current = Some(magi::daemon::Current {
            task: "20260902-140501-t111".to_owned(),
            run: "20260902-140502-r111".to_owned(),
        });
        magi::daemon::write_status_to(&dir.path().join("daemon.json"), &status).unwrap();

        let text = doctor_queue_and_loop(dir.path());

        assert!(text.contains("loop       running (pid 4242)"), "{text}");
        assert!(
            text.contains("working  task 20260902-140501-t111 (run 20260902-140502-r111)"),
            "{text}"
        );
    }

    #[test]
    fn a_stale_daemon_file_reports_not_running_rather_than_work_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let mut status = magi::daemon::Status::new();
        status.updated_at = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(60);
        status.current = Some(magi::daemon::Current {
            task: "20260902-140501-t111".to_owned(),
            run: "20260902-140502-r111".to_owned(),
        });
        magi::daemon::write_status_to(&dir.path().join("daemon.json"), &status).unwrap();

        let text = doctor_queue_and_loop(dir.path());

        assert!(
            text.contains("loop       not running"),
            "a stale heartbeat must not claim work is in flight: {text}"
        );
        assert!(!text.contains("20260902-140501-t111"), "{text}");
    }

    #[test]
    fn doctor_counts_runs_this_build_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join("runs");
        std::fs::create_dir_all(runs.join("20260902-140502-bad")).unwrap();
        std::fs::write(
            runs.join("20260902-140502-bad").join("run.json"),
            "{ truncated",
        )
        .unwrap();

        let text = doctor_queue_and_loop(dir.path());

        assert!(text.contains("unreadable 1"), "{text}");
    }

    #[test]
    fn a_mistyped_subcommand_never_becomes_a_task() {
        let words = subcommand_words();
        // The words that actually bit: `run show` does not exist, so every
        // word became the instruction and a competition started.
        assert!(
            words.contains("show"),
            "the guard reads clap's own commands"
        );
        assert!(words.contains("rm"));
        assert!(words.contains("task"));

        let typo: Vec<String> = ["show", "3cbf"].iter().map(|s| (*s).to_owned()).collect();
        let why = mistyped_command(&typo, false, &words).expect("refused");
        assert!(why.contains("`show` names a magi subcommand"), "{why}");
        assert!(
            why.contains("magi run -- show 3cbf"),
            "the error must show the way through: {why}"
        );

        // `--` is the way through, and it is honoured.
        assert!(mistyped_command(&typo, true, &words).is_none());

        // A real instruction is untouched, whatever it says about runs.
        let real: Vec<String> = "delete the runs nobody wants any more"
            .split(' ')
            .map(ToOwned::to_owned)
            .collect();
        assert!(mistyped_command(&real, false, &words).is_none());

        // Nothing to run is not this guard's business; clap already says so.
        assert!(mistyped_command(&[], false, &words).is_none());
    }

    #[test]
    fn task_done_is_its_own_verb_and_not_release() {
        // These two are one keystroke apart and do opposite things: `done`
        // finishes a task, `release` puts it back in line and pays for the
        // whole competition again. Wiring `done` to `release` by accident is
        // exactly how work already merged into `main` gets re-competed.
        let done = Cli::try_parse_from(["magi", "task", "done", "199c"]).unwrap();
        match done.command {
            Some(Command::Task {
                command: TaskCmd::Done { id },
            }) => assert_eq!(id, "199c"),
            other => panic!("expected TaskCmd::Done, got {other:?}"),
        }

        let released = Cli::try_parse_from(["magi", "task", "release", "199c"]).unwrap();
        assert!(matches!(
            released.command,
            Some(Command::Task {
                command: TaskCmd::Release { .. }
            })
        ));

        // And the transitions they stand for really are opposite.
        let mut t = magi::queue::Task::new(
            "landed by hand".to_owned(),
            "do it".to_owned(),
            PathBuf::from("/repo"),
            magi::queue::Source::Human,
        );
        t.start("20260903-134458-ec12".to_owned());
        t.succeed();
        assert_eq!(t.status, magi::queue::TaskStatus::Done);
        assert!(!t.status.runnable(), "a finished task is not offered again");
        t.release();
        assert!(t.status.runnable(), "and release is the way back in");
    }

    #[test]
    fn run_rm_cli_argument_parsing() {
        // 1. magi run rm <id> succeeds and parses id
        let parsed = Cli::try_parse_from(["magi", "run", "rm", "20260902-140501-a1b2"]).unwrap();
        match parsed.command {
            Some(Command::Run {
                command: Some(RunCmd::Rm { id, .. }),
                ..
            }) => {
                assert_eq!(id, vec!["20260902-140501-a1b2"]);
            }
            other => panic!("expected RunCmd::Rm, got {other:?}"),
        }

        // 2. magi run rm without id fails
        let err = Cli::try_parse_from(["magi", "run", "rm"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "omitting id from magi run rm must fail clap parsing: {err}"
        );

        // 3. magi run <instruction> still parses instruction normally
        let parsed_normal = Cli::try_parse_from(["magi", "run", "fix", "a", "bug"]).unwrap();
        match parsed_normal.command {
            Some(Command::Run {
                command: None,
                instruction,
                ..
            }) => {
                assert_eq!(instruction, vec!["fix", "a", "bug"]);
            }
            other => panic!("expected normal Command::Run, got {other:?}"),
        }

        // 4. an instruction that merely starts with "rm" must still parse —
        // clap only recognises the literal token "rm" as the subcommand, it
        // cannot know in advance that this is prose, not a run id.
        let parsed_prose =
            Cli::try_parse_from(["magi", "run", "rm", "this", "is", "a", "task"]).unwrap();
        match parsed_prose.command {
            Some(Command::Run {
                command: Some(RunCmd::Rm { id, .. }),
                ..
            }) => {
                assert_eq!(id, vec!["this", "is", "a", "task"]);
            }
            other => panic!("expected RunCmd::Rm carrying the prose, got {other:?}"),
        }

        // 5. a flag placed after rm-led prose must still parse and carry its
        // value, not just avoid erroring: clap only declares --candidates on
        // Run itself, so it has to be declared on RunCmd::Rm too.
        let parsed_flag_after = Cli::try_parse_from([
            "magi",
            "run",
            "rm",
            "the",
            "dead",
            "code",
            "--candidates",
            "3",
        ])
        .unwrap();
        match parsed_flag_after.command {
            Some(Command::Run {
                command: Some(RunCmd::Rm { id, opts }),
                ..
            }) => {
                assert_eq!(id, vec!["the", "dead", "code"]);
                assert_eq!(opts.candidates, Some(3));
            }
            other => panic!("expected RunCmd::Rm carrying --candidates, got {other:?}"),
        }

        // 6. a flag before "rm" must still reach the instruction path even
        // though a different flag also appears after the prose — the two
        // sides are parsed into separate RunOpts and must be merged rather
        // than one clobbering the other.
        let parsed_flag_both_sides = Cli::try_parse_from([
            "magi",
            "run",
            "--candidates",
            "3",
            "rm",
            "the",
            "dead",
            "code",
            "--seed",
            "7",
        ])
        .unwrap();
        match parsed_flag_both_sides.command {
            Some(Command::Run {
                command: Some(RunCmd::Rm { id, opts: rm_opts }),
                opts,
                ..
            }) => {
                assert_eq!(id, vec!["the", "dead", "code"]);
                assert_eq!(opts.candidates, Some(3));
                assert_eq!(rm_opts.seed, Some(7));
                let merged = rm_opts.merge(opts);
                assert_eq!(merged.candidates, Some(3));
                assert_eq!(merged.seed, Some(7));
            }
            other => panic!("expected RunCmd::Rm with flags on both sides, got {other:?}"),
        }
    }

    #[test]
    fn run_rm_cmd_guards_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        // `set_home` rather than setting `MAGI_HOME`: this binary runs its
        // tests as threads in one process, and an environment variable written
        // from a test is read by every other thread - including the ones that
        // already resolved the operator's real data directory. The pin is the
        // mechanism `run::home` documents for exactly this.
        magi::run::set_home(dir.path().to_path_buf());
        let runs = dir.path().join("runs");
        std::fs::create_dir_all(&runs).unwrap();

        // 1. Running run fails to delete
        let run_running = "20260901-000000-rung";
        let dir_rung = runs.join(run_running);
        std::fs::create_dir_all(&dir_rung).unwrap();
        let mut s_rung = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc".to_owned(),
            "inst".to_owned(),
            Config::default(),
        );
        s_rung.id = run_running.to_owned();
        s_rung.status = RunStatus::Prep;
        std::fs::write(
            dir_rung.join("run.json"),
            serde_json::to_string(&s_rung).unwrap(),
        )
        .unwrap();

        // A heartbeat naming this run is what makes it refusable: an
        // unfinished run with no live daemon behind it is a leftover from a
        // killed process, and deleting those is the point of the command.
        let mut beat = magi::daemon::Status::new();
        beat.current = Some(magi::daemon::Current {
            task: "20260901-000000-task".to_owned(),
            run: run_running.to_owned(),
        });
        beat.updated_at = jiff::Timestamp::now();
        magi::daemon::write_status_to(&dir.path().join("daemon.json"), &beat).unwrap();

        let err = run_rm_cmd(run_running).unwrap_err().to_string();
        assert!(err.contains("live daemon"), "{err}");
        assert!(dir_rung.exists(), "a run in flight must be kept");

        // 2. Unfolded run fails to delete and suggests magi fold
        let run_unfolded = "20260901-000000-unfd";
        let dir_unfd = runs.join(run_unfolded);
        std::fs::create_dir_all(&dir_unfd).unwrap();
        let mut s_unfd = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abc".to_owned(),
            "inst".to_owned(),
            Config::default(),
        );
        s_unfd.id = run_unfolded.to_owned();
        s_unfd.status = RunStatus::Merged;
        s_unfd.candidates.push(magi::run::Candidate {
            index: 0,
            label: 'A',
            agent: "a".to_owned(),
            branch: "b".to_owned(),
            worktree: PathBuf::from("/w"),
            summary: String::new(),
            stat: String::new(),
            files: 1,
            commits: 1,
            empty: false,
            failed: None,
            duration_ms: 0,
            folded: false,
        });
        std::fs::write(
            dir_unfd.join("run.json"),
            serde_json::to_string(&s_unfd).unwrap(),
        )
        .unwrap();

        let err = run_rm_cmd(run_unfolded).unwrap_err().to_string();
        assert!(err.contains("magi fold"), "{err}");
        assert!(dir_unfd.exists(), "unfolded run must be kept");

        // 3. Finished and folded run succeeds
        s_unfd.candidates[0].folded = true;
        std::fs::write(
            dir_unfd.join("run.json"),
            serde_json::to_string(&s_unfd).unwrap(),
        )
        .unwrap();
        std::fs::create_dir_all(dir_unfd.join("artifacts")).unwrap();
        std::fs::write(dir_unfd.join("artifacts").join("diff.patch"), "patch").unwrap();

        assert!(
            run_rm_cmd("unfd").is_ok(),
            "prefix/suffix resolution succeeds"
        );
        assert!(!dir_unfd.exists(), "run directory is removed");
    }
}
