//! The observation deck.
//!
//! A competition takes minutes of agent latency per node, across several runs
//! at once. Watching that with `magi show` in a loop is the "walking the
//! terminal tabs" problem the whole design exists to remove, so bare `magi`
//! opens this instead: every run in one list, status in colour, the selected
//! run's full report beside it, refreshed from disk as the graph writes.
//!
//! It is **read-only on purpose**. The runs are the record of what the agents
//! did; a keystroke that could rewrite one belongs in an explicit subcommand
//! (`magi fold`), not one `j` away from browsing.
//!
//! # Structure
//!
//! [`App`] is pure state with pure transitions, so the interesting behaviour —
//! selection clamping, filter cycling, keeping the cursor on the same run
//! across a refresh — is unit-testable without a terminal. [`draw`] is the only
//! function that knows about ratatui, and [`run`] is the only one that touches
//! the real terminal.
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use ansi_to_tui::IntoText;
use anyhow::{Context as _, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::report;
use crate::run::{self, RunState, RunStatus};

/// How often the run list is re-read from disk.
const REFRESH: Duration = Duration::from_millis(1000);
/// How long a keypress wait blocks before the loop reconsiders refreshing.
const TICK: Duration = Duration::from_millis(200);

/// Which pane the keys move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The run list.
    List,
    /// The report pane.
    Detail,
}

/// Which runs to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Everything on disk.
    All,
    /// Still walking the graph.
    Active,
    /// Merged or gate-green.
    Done,
    /// Blocked or failed — the ones that want a human.
    Attention,
}

impl Filter {
    /// Cycle order for the `a` key.
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Active,
            Self::Active => Self::Attention,
            Self::Attention => Self::Done,
            Self::Done => Self::All,
        }
    }

    /// Label for the header.
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Active => "active",
            Self::Done => "done",
            Self::Attention => "attention",
        }
    }

    /// Does `status` belong in this filter?
    pub fn accepts(self, status: RunStatus) -> bool {
        match self {
            Self::All => true,
            Self::Active => !status.done(),
            Self::Done => matches!(status, RunStatus::Merged | RunStatus::Ready),
            Self::Attention => matches!(status, RunStatus::Blocked | RunStatus::Failed),
        }
    }
}

/// One loaded run plus the mtime it was loaded at.
#[derive(Debug, Clone)]
struct Loaded {
    id: String,
    mtime: Option<SystemTime>,
    state: RunState,
}

/// Counts for the header.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Runs on disk.
    pub total: usize,
    /// Still walking the graph.
    pub active: usize,
    /// Merged or ready.
    pub done: usize,
    /// Blocked or failed.
    pub attention: usize,
    /// State files that could not be parsed.
    pub unreadable: usize,
}

/// TUI state.
pub struct App {
    runs: Vec<Loaded>,
    /// Index into [`App::visible`], not into `runs`.
    cursor: usize,
    /// Vertical scroll of the report pane.
    scroll: u16,
    focus: Focus,
    filter: Filter,
    /// Runs on disk whose state file could not be parsed at all.
    unreadable: usize,
    help: bool,
    status: Option<String>,
    last_refresh: Instant,
    /// Set by `q` / `Esc` / `Ctrl-C`.
    quit: bool,
}

impl App {
    /// Build from already-loaded runs. Used by the tests; [`App::load`] is what
    /// the binary calls.
    pub fn new(states: Vec<RunState>) -> Self {
        let runs = states
            .into_iter()
            .map(|state| Loaded {
                id: state.id.clone(),
                mtime: None,
                state,
            })
            .collect();
        Self {
            runs,
            cursor: 0,
            scroll: 0,
            focus: Focus::List,
            filter: Filter::All,
            unreadable: 0,
            help: false,
            status: None,
            last_refresh: Instant::now(),
            quit: false,
        }
    }

    /// Build by reading every run on disk.
    pub fn load() -> Self {
        let mut app = Self::new(Vec::new());
        app.refresh();
        app
    }

    /// Re-read the run directory, keeping the cursor on the same run.
    ///
    /// Only files whose mtime moved are parsed again: with a few hundred runs
    /// on disk, re-parsing all of them every second would be the most
    /// expensive thing magi does while sitting idle.
    ///
    /// A run that fails to parse does **not** disappear. Dropping it would make
    /// a row blink out of a live view every time a load failed — and worse, a
    /// permanently unreadable run (a state file from a different schema) would
    /// be invisible here while `magi list` reports it as unreadable. So the last
    /// good snapshot is kept if there is one, and otherwise the run is counted
    /// and surfaced in the header.
    pub fn refresh(&mut self) {
        let selected_id = self.selected().map(|s| s.id.clone());
        let ids = run::list_ids();
        let mut next: Vec<Loaded> = Vec::with_capacity(ids.len());
        let mut unreadable = 0usize;
        for id in ids {
            let mtime = state_mtime(&id);
            let previous = self.runs.iter().find(|l| l.id == id);
            if let Some(l) = previous.filter(|l| l.mtime == mtime && mtime.is_some()) {
                next.push(l.clone());
                continue;
            }
            match RunState::load(&id) {
                Ok(state) => next.push(Loaded { id, mtime, state }),
                Err(_) => match previous {
                    Some(stale) => next.push(stale.clone()),
                    None => unreadable += 1,
                },
            }
        }
        self.runs = next;
        self.unreadable = unreadable;
        self.last_refresh = Instant::now();
        // Follow the run the cursor was on; fall back to clamping.
        if let Some(id) = selected_id
            && let Some(pos) = self.visible().iter().position(|i| self.runs[*i].id == id)
        {
            self.cursor = pos;
        }
        self.clamp();
    }

    /// Indices into `runs` that pass the filter.
    pub fn visible(&self) -> Vec<usize> {
        self.runs
            .iter()
            .enumerate()
            .filter(|(_, l)| self.filter.accepts(l.state.status))
            .map(|(i, _)| i)
            .collect()
    }

    /// The selected run, if any.
    pub fn selected(&self) -> Option<&RunState> {
        let visible = self.visible();
        visible.get(self.cursor).map(|i| &self.runs[*i].state)
    }

    /// Status counts across everything on disk, filter-independent.
    pub fn counts(&self) -> Counts {
        let mut c = Counts {
            total: self.runs.len(),
            unreadable: self.unreadable,
            ..Counts::default()
        };
        for l in &self.runs {
            match l.state.status {
                RunStatus::Merged | RunStatus::Ready => c.done += 1,
                RunStatus::Blocked | RunStatus::Failed => c.attention += 1,
                _ => c.active += 1,
            }
        }
        c
    }

    fn clamp(&mut self) {
        let len = self.visible().len();
        self.cursor = if len == 0 {
            0
        } else {
            self.cursor.min(len - 1)
        };
    }

    /// Move the list cursor down.
    pub fn next_run(&mut self) {
        let len = self.visible().len();
        if len > 0 {
            self.cursor = (self.cursor + 1) % len;
            self.scroll = 0;
        }
    }

    /// Move the list cursor up.
    pub fn prev_run(&mut self) {
        let len = self.visible().len();
        if len > 0 {
            self.cursor = (self.cursor + len - 1) % len;
            self.scroll = 0;
        }
    }

    /// Jump to the newest run.
    pub fn first_run(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    /// Jump to the oldest run.
    pub fn last_run(&mut self) {
        self.cursor = self.visible().len().saturating_sub(1);
        self.scroll = 0;
    }

    /// Scroll the report pane, clamped to the range ratatui's offset accepts.
    pub fn scroll_by(&mut self, delta: i32) {
        let next = i32::from(self.scroll).saturating_add(delta);
        self.scroll = next.clamp(0, i32::from(u16::MAX)) as u16;
    }

    /// Cycle the filter, keeping the cursor in range.
    pub fn cycle_filter(&mut self) {
        self.filter = self.filter.next();
        self.cursor = 0;
        self.scroll = 0;
        self.status = Some(format!("filter: {}", self.filter.label()));
    }

    /// Swap which pane the movement keys drive.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::List => Focus::Detail,
            Focus::Detail => Focus::List,
        };
    }

    /// Current filter.
    pub fn filter(&self) -> Filter {
        self.filter
    }

    /// Current focus.
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// Should the loop exit?
    pub fn quitting(&self) -> bool {
        self.quit
    }

    /// The report text for the selected run, ANSI colours included.
    fn detail(&self) -> String {
        match self.selected() {
            Some(state) => report::run(state),
            None => String::from("no runs yet\n\nrun `magi run \"<task>\"` in a repository."),
        }
    }

    /// Apply one key press.
    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.status = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Quit is checked before anything modal can intercept it. A help
        // overlay that eats Ctrl-C is how a TUI earns a reputation for
        // trapping people.
        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (ctrl && matches!(key.code, KeyCode::Char('c')))
        {
            self.quit = true;
            return;
        }

        // Help is modal: any other key closes it and does nothing else, so a
        // keystroke aimed at the overlay never leaks into the panes behind it.
        if self.help {
            self.help = false;
            return;
        }

        match key.code {
            KeyCode::Char('?') => self.help = true,
            KeyCode::Tab | KeyCode::BackTab => self.toggle_focus(),
            KeyCode::Char('a') => self.cycle_filter(),
            KeyCode::Char('r') => {
                self.refresh();
                self.status = Some("refreshed".to_owned());
            }
            KeyCode::Char('o') => self.open_selected(),
            KeyCode::Char('g') | KeyCode::Home => self.first_run(),
            KeyCode::Char('G') | KeyCode::End => self.last_run(),
            KeyCode::Char('J') => self.scroll_by(5),
            KeyCode::Char('K') => self.scroll_by(-5),
            KeyCode::PageDown => self.scroll_by(20),
            KeyCode::PageUp => self.scroll_by(-20),
            KeyCode::Char('j') | KeyCode::Down => match self.focus {
                Focus::List => self.next_run(),
                Focus::Detail => self.scroll_by(1),
            },
            KeyCode::Char('k') | KeyCode::Up => match self.focus {
                Focus::List => self.prev_run(),
                Focus::Detail => self.scroll_by(-1),
            },
            _ => {}
        }
    }

    /// Hand the run's directory to the OS opener. Read-only: it reveals the
    /// artifacts, it does not change them.
    fn open_selected(&mut self) {
        let Some(dir) = self.selected().map(|s| s.dir()) else {
            return;
        };
        self.status = Some(match open_path(&dir) {
            Ok(()) => format!("opened {}", dir.display()),
            Err(e) => format!("could not open {}: {e}", dir.display()),
        });
    }

    /// Refresh if the interval has elapsed.
    fn tick(&mut self) {
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh();
        }
    }
}

fn state_mtime(id: &str) -> Option<SystemTime> {
    std::fs::metadata(run::run_dir(id).join("run.json"))
        .and_then(|m| m.modified())
        .ok()
}

#[cfg(windows)]
fn open_path(path: &Path) -> Result<()> {
    std::process::Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .context("spawn explorer")
}

#[cfg(target_os = "macos")]
fn open_path(path: &Path) -> Result<()> {
    std::process::Command::new("open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .context("spawn open")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_path(path: &Path) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .context("spawn xdg-open")
}

/// Colour for a status word in the list.
fn status_style(status: RunStatus) -> Style {
    match status {
        RunStatus::Merged => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        RunStatus::Ready => Style::default().fg(Color::Green),
        RunStatus::Blocked => Style::default().fg(Color::Yellow),
        RunStatus::Failed => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::Cyan),
    }
}

/// Render one frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    header(frame, chunks[0], app);
    body(frame, chunks[1], app);
    footer(frame, chunks[2], app);

    if app.help {
        help_overlay(frame, frame.area());
    }
}

fn header(frame: &mut Frame, area: Rect, app: &App) {
    let c = app.counts();
    let mut line = Line::from(vec![
        Span::styled(
            " magi ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {} runs  ", c.total)),
        Span::styled(
            format!("{} active", c.active),
            status_style(RunStatus::Prep),
        ),
        Span::raw("  "),
        Span::styled(format!("{} done", c.done), status_style(RunStatus::Ready)),
        Span::raw("  "),
        Span::styled(
            format!("{} attention", c.attention),
            status_style(RunStatus::Blocked),
        ),
        Span::raw(format!("  |  filter: {}", app.filter.label())),
    ]);
    if c.unreadable > 0 {
        line.push_span(Span::styled(
            format!("  |  {} unreadable", c.unreadable),
            status_style(RunStatus::Failed),
        ));
    }
    frame.render_widget(Paragraph::new(line), area);
}

fn body(frame: &mut Frame, area: Rect, app: &mut App) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);

    let visible = app.visible();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|i| {
            let state = &app.runs[*i].state;
            let status = format!("{:?}", state.status).to_lowercase();
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<12}", status), status_style(state.status)),
                Span::raw(format!(
                    "{}  {}",
                    state.short(),
                    state.instruction.lines().next().unwrap_or_default()
                )),
            ]))
        })
        .collect();

    let list_focused = app.focus == Focus::List;
    let list = List::new(items)
        .block(pane_block(" runs ", list_focused))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    let mut list_state = ListState::default();
    if !visible.is_empty() {
        list_state.select(Some(app.cursor));
    }
    frame.render_stateful_widget(list, panes[0], &mut list_state);

    // `report::run` already renders every field with colour; parsing its ANSI
    // back into spans keeps one implementation of the report instead of two.
    let text = app
        .detail()
        .into_text()
        .unwrap_or_else(|_| app.detail().into());
    let detail = Paragraph::new(text)
        .block(pane_block(" report ", !list_focused))
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0));
    frame.render_widget(detail, panes[1]);
}

fn pane_block(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::bordered().title(title).border_style(style)
}

fn footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = match &app.status {
        Some(msg) => msg.clone(),
        None => "j/k move  Tab pane  J/K scroll  a filter  r refresh  o open dir  ? help  q quit"
            .to_owned(),
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray))),
        area,
    );
}

fn help_overlay(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from("magi — observation deck (read-only)"),
        Line::from(""),
        Line::from("j / k / ↓ / ↑   move in the focused pane"),
        Line::from("Tab             switch pane (runs / report)"),
        Line::from("J / K           scroll the report by 5"),
        Line::from("PageDown / Up   scroll the report by 20"),
        Line::from("g / G           newest / oldest run"),
        Line::from("a               cycle filter: all, active, attention, done"),
        Line::from("r               refresh now (it also refreshes every second)"),
        Line::from("o               open the run's directory in the OS file manager"),
        Line::from("q / Esc         quit"),
        Line::from(""),
        Line::from("Nothing here mutates a run. Use `magi fold` for cleanup."),
    ];
    let height = (lines.len() as u16 + 2).min(area.height);
    let width = 66.min(area.width);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(pane_block(" help ", true)),
        popup,
    );
}

/// RAII guard for raw mode and the alternate screen.
///
/// A guard rather than a cleanup block, so a panic anywhere inside the loop
/// still gives the terminal back.
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode().context("enabling terminal raw mode")?;
        execute!(io::stdout(), EnterAlternateScreen).context("entering alt screen")?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Reverse of `new`, with `disable_raw_mode` LAST. On Windows the
        // console-mode restore performed while leaving the alternate screen is
        // taken from a snapshot captured after raw mode was enabled, so
        // disabling raw mode first lets that restore put the cooked bits back
        // to their raw values — stranding the whole console in raw mode after
        // magi exits. Learned in yukimemi/shoka.
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        let _ = disable_raw_mode();
    }
}

/// Open the observation deck on the real terminal.
pub fn run() -> Result<()> {
    let _guard = TerminalGuard::new()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
    .context("constructing ratatui terminal")?;
    let mut app = App::load();
    event_loop(&mut terminal, &mut app)
}

/// The loop, generic over the backend so a test can drive it.
pub fn event_loop<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    while !app.quitting() {
        terminal
            .draw(|f| draw(f, app))
            .map_err(|e| anyhow::anyhow!("drawing frame: {e}"))?;
        if event::poll(TICK).context("polling for input")?
            && let Event::Key(key) = event::read().context("reading input")?
        {
            app.on_key(key);
        }
        app.tick();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::run::Tally;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn state(instruction: &str, status: RunStatus) -> RunState {
        let mut s = RunState::new(
            PathBuf::from("/repo"),
            "main".to_owned(),
            "abcdef1234".to_owned(),
            instruction.to_owned(),
            Config::default(),
        );
        s.status = status;
        s
    }

    fn app() -> App {
        App::new(vec![
            state("add retries", RunStatus::Reviewing),
            state("fix the parser", RunStatus::Blocked),
            state("document the gate", RunStatus::Merged),
        ])
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn counts_partition_every_run() {
        let c = app().counts();
        assert_eq!(c.total, 3);
        assert_eq!(c.active, 1);
        assert_eq!(c.attention, 1);
        assert_eq!(c.done, 1);
        assert_eq!(c.active + c.attention + c.done, c.total);
    }

    /// A corrupt state file must not make a row blink out of a live view.
    ///
    /// Uses a temp run home so it never touches the operator's history. The
    /// home is process-global and set once, so this is the only lib test that
    /// reads from disk.
    #[test]
    fn an_unreadable_run_keeps_its_last_snapshot_and_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        run::set_home(dir.path().to_path_buf());
        // If another test already pinned the home, this one has nothing to say.
        if run::home() != dir.path() {
            return;
        }

        let mut saved = state("watch me", RunStatus::Reviewing);
        saved.save().expect("save run state");
        let id = saved.id.clone();

        let mut a = App::load();
        assert_eq!(a.visible().len(), 1, "the saved run is listed");
        assert_eq!(a.counts().unreadable, 0);

        // Corrupt it and force a reload: the row stays, with the old snapshot.
        let path = run::run_dir(&id).join("run.json");
        std::fs::write(&path, "{ not json").unwrap();
        a.refresh();
        assert_eq!(a.visible().len(), 1, "row must not blink out");
        assert_eq!(a.selected().unwrap().instruction, "watch me");
        assert_eq!(a.counts().unreadable, 0, "a stale snapshot is not a loss");

        // A fresh reader has no snapshot to fall back on, so it must say so
        // rather than pretend the run does not exist.
        let fresh = App::load();
        assert!(fresh.visible().is_empty());
        assert_eq!(fresh.counts().unreadable, 1);
        assert_eq!(fresh.counts().total, 0);
    }

    #[test]
    fn cursor_wraps_in_both_directions() {
        let mut a = app();
        assert_eq!(a.selected().unwrap().instruction, "add retries");
        a.next_run();
        a.next_run();
        assert_eq!(a.selected().unwrap().instruction, "document the gate");
        a.next_run();
        assert_eq!(a.selected().unwrap().instruction, "add retries");
        a.prev_run();
        assert_eq!(a.selected().unwrap().instruction, "document the gate");
    }

    #[test]
    fn filter_cycles_and_narrows() {
        let mut a = app();
        assert_eq!(a.visible().len(), 3);
        a.cycle_filter();
        assert_eq!(a.filter(), Filter::Active);
        assert_eq!(a.visible().len(), 1);
        assert_eq!(a.selected().unwrap().instruction, "add retries");
        a.cycle_filter();
        assert_eq!(a.filter(), Filter::Attention);
        assert_eq!(a.selected().unwrap().instruction, "fix the parser");
        a.cycle_filter();
        assert_eq!(a.filter(), Filter::Done);
        assert_eq!(a.selected().unwrap().instruction, "document the gate");
        a.cycle_filter();
        assert_eq!(a.filter(), Filter::All);
    }

    #[test]
    fn a_filter_that_hides_the_cursor_does_not_panic() {
        let mut a = app();
        a.last_run();
        a.filter = Filter::Active;
        a.clamp();
        assert!(a.selected().is_some());
        a.filter = Filter::Done;
        a.cursor = 99;
        a.clamp();
        assert_eq!(a.cursor, 0);
    }

    #[test]
    fn empty_state_selects_nothing_and_still_renders() {
        let mut a = App::new(Vec::new());
        assert!(a.selected().is_none());
        a.next_run();
        a.prev_run();
        a.last_run();
        assert_eq!(a.cursor, 0);
        assert!(a.detail().contains("no runs yet"));
    }

    #[test]
    fn scroll_never_goes_negative() {
        let mut a = app();
        a.scroll_by(-10);
        assert_eq!(a.scroll, 0);
        a.scroll_by(7);
        assert_eq!(a.scroll, 7);
        a.scroll_by(-3);
        assert_eq!(a.scroll, 4);
    }

    #[test]
    fn focus_routes_movement_keys() {
        let mut a = app();
        assert_eq!(a.focus(), Focus::List);
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.selected().unwrap().instruction, "fix the parser");
        assert_eq!(a.scroll, 0);

        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.focus(), Focus::Detail);
        a.on_key(key(KeyCode::Char('j')));
        // Same run, scrolled instead.
        assert_eq!(a.selected().unwrap().instruction, "fix the parser");
        assert_eq!(a.scroll, 1);
    }

    #[test]
    fn quit_keys() {
        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut a = app();
            a.on_key(key(code));
            assert!(a.quitting(), "{code:?} should quit");
        }
        let mut a = app();
        a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(a.quitting());
        // A bare `c` is not a quit.
        let mut a = app();
        a.on_key(key(KeyCode::Char('c')));
        assert!(!a.quitting());
    }

    #[test]
    fn help_is_modal_but_never_swallows_a_quit() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('?')));
        assert!(a.help);
        a.on_key(key(KeyCode::Char('j')));
        assert!(!a.help, "any key dismisses help");
        // Dismissal must not also move the cursor.
        assert_eq!(a.selected().unwrap().instruction, "add retries");

        // `?` closes it too, rather than toggling twice back open.
        a.on_key(key(KeyCode::Char('?')));
        a.on_key(key(KeyCode::Char('?')));
        assert!(!a.help);

        for code in [KeyCode::Char('q'), KeyCode::Esc] {
            let mut a = app();
            a.on_key(key(KeyCode::Char('?')));
            a.on_key(key(code));
            assert!(a.quitting(), "{code:?} must quit from the help overlay");
        }
        let mut a = app();
        a.on_key(key(KeyCode::Char('?')));
        a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(a.quitting(), "help must not swallow Ctrl-C");
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut a = app();
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        a.on_key(release);
        assert!(!a.quitting(), "a key release must not act twice");
    }

    #[test]
    fn frame_shows_counts_list_and_report() {
        let mut a = app();
        a.runs[0].state.tally = Some(Tally {
            first_choice: BTreeMap::from([('A', 3)]),
            borda: BTreeMap::new(),
            winner: 'A',
            rankings: 3,
            unanimous_initial: true,
            deliberated: false,
            changed_votes: 0,
            unanimous_final: true,
            tie_break: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut a)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("3 runs"), "{rendered}");
        assert!(rendered.contains("1 active"));
        assert!(rendered.contains("1 attention"));
        assert!(rendered.contains("reviewing"), "status word in the list");
        assert!(rendered.contains("add retries"), "instruction in the list");
        assert!(rendered.contains("blocked"));
        // The report pane is the real `report::run` output.
        assert!(rendered.contains("candidates"), "report pane rendered");
        assert!(rendered.contains("q quit"), "footer hints");
    }

    #[test]
    fn help_overlay_renders_over_the_panes() {
        let mut a = app();
        a.on_key(key(KeyCode::Char('?')));
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut a)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("observation deck"));
        assert!(rendered.contains("Nothing here mutates a run"));
    }

    #[test]
    fn a_narrow_terminal_still_renders() {
        let mut a = app();
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).unwrap();
        terminal.draw(|f| draw(f, &mut a)).unwrap();
        a.on_key(key(KeyCode::Char('?')));
        terminal.draw(|f| draw(f, &mut a)).unwrap();
    }
}
