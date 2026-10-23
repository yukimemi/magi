//! Causal block/release handshake for tests that act while a mock seat is in flight.
//!
//! Instead of polling for `started-<seat>` with a fixed wall-clock cap (which
//! flakes whenever `prep`, worktree creation, or Windows MSYS shell startup
//! takes longer than the cap) or letting the mock silently continue after N
//! polls, [`SeatBlock::started`] races the marker check against the run under
//! test and [`SeatBlock::drop`] writes `abandon-<seat>` if the test exits
//! without calling [`SeatBlock::release`].

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use magi::queue::{Queue, Task, TaskStatus};
use magi::run::RunState;

use super::Fixture;

/// Controls a single mock seat configured to block in [`super::MOCK`].
pub struct SeatBlock {
    seat: String,
    node: String,
    started_marker: PathBuf,
    release_marker: PathBuf,
    abandon_marker: PathBuf,
    released: AtomicBool,
}

impl SeatBlock {
    /// Configure every agent in `fx` to block `seat` when it enters `node`.
    pub fn install(fx: &mut Fixture, seat: &str, node: &str) -> Self {
        let dir = fx.tmp.path().join("block");
        std::fs::create_dir_all(&dir).expect("create block dir");
        let started_marker = dir.join(format!("started-{seat}"));
        let release_marker = dir.join(format!("release-{seat}"));
        let abandon_marker = dir.join(format!("abandon-{seat}"));
        for marker in [&started_marker, &release_marker, &abandon_marker] {
            let _ = std::fs::remove_file(marker);
        }
        let dir_str = dir.to_string_lossy().into_owned();
        for a in &mut fx.config.agents {
            a.env.insert("MOCK_BLOCK_SEAT".to_owned(), seat.to_owned());
            a.env.insert("MOCK_BLOCK_NODE".to_owned(), node.to_owned());
            a.env.insert("MOCK_BLOCK_DIR".to_owned(), dir_str.clone());
        }
        Self {
            seat: seat.to_owned(),
            node: node.to_owned(),
            started_marker,
            release_marker,
            abandon_marker,
            released: AtomicBool::new(false),
        }
    }

    /// Path to the `started-<seat>` marker written by the mock.
    pub fn started_path(&self) -> &std::path::Path {
        &self.started_marker
    }

    /// Path to the `abandon-<seat>` marker written on drop when unreleased.
    pub fn abandon_path(&self) -> &std::path::Path {
        &self.abandon_marker
    }

    /// Wait until the mock seat writes `started-<seat>`, racing `run` and `ended`.
    ///
    /// Has no poll-count or wall-clock cap: if the seat never starts because
    /// the run finishes or settles early, panics immediately with the run's
    /// outcome or `ended`'s diagnostic. If both the marker and termination are
    /// observable on the same tick, termination wins.
    pub async fn started<F, D, E>(&self, run: &mut F, ended: E)
    where
        F: Future<Output = D> + Unpin,
        D: std::fmt::Debug,
        E: FnMut() -> Option<String>,
    {
        let what = format!(
            "the mock never signalled that {} had started in {}",
            self.seat, self.node
        );
        wait_while_running(run, || self.started_marker.exists(), ended, &what).await;
    }

    /// Unblock the waiting mock seat by writing `release-<seat>`.
    pub fn release(&self) {
        std::fs::write(&self.release_marker, b"go")
            .unwrap_or_else(|e| panic!("release {}: {e}", self.seat));
        self.released.store(true, Ordering::SeqCst);
    }
}

impl Drop for SeatBlock {
    fn drop(&mut self) {
        if !self.released.load(Ordering::SeqCst) {
            let _ = std::fs::write(&self.abandon_marker, b"abandon");
        }
    }
}

/// Poll `ready` every 10ms while driving `run` and checking `ended`.
///
/// Termination takes precedence over `ready`: on each wakeup `run` is polled
/// first (`biased;`), and on each tick `ended` (and `run`) is checked both
/// before and after `ready`.
pub async fn wait_while_running<F, D, R, E>(run: &mut F, mut ready: R, mut ended: E, what: &str)
where
    F: Future<Output = D> + Unpin,
    D: std::fmt::Debug,
    R: FnMut() -> bool,
    E: FnMut() -> Option<String>,
{
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    loop {
        tokio::select! {
            biased;
            out = &mut *run => {
                panic!("{what}: run finished early with {out:?}");
            }
            _ = tick.tick() => {
                if let Some(reason) = ended() {
                    panic!("{what}: {reason}");
                }
                if ready() {
                    if let std::task::Poll::Ready(out) = std::future::poll_fn(|cx| {
                        std::task::Poll::Ready(std::pin::Pin::new(&mut *run).poll(cx))
                    })
                    .await
                    {
                        panic!("{what}: run finished early with {out:?}");
                    }
                    if let Some(reason) = ended() {
                        panic!("{what}: {reason}");
                    }
                    return;
                }
            }
        }
    }
}

/// Format a diagnostic if `task_id` has already settled out of its active run.
///
/// Transient `TaskStatus::Queued` (before the daemon claims the task) and an
/// active first attempt (`TaskStatus::Running` with no settled pass and a
/// live run) return `None`. Any terminal or settled status (`Done`, `Failed`,
/// `Held`, `Blocked`, `Parked`), a completed pass, a retry (`attempts > 1`),
/// or a run whose `RunState` has already exited returns `Some` with the task's
/// status, `last_error`, and `RunState`.
pub fn task_ended(queue: &Queue, task_id: &str) -> Option<String> {
    let task = match queue.get(task_id) {
        Ok(t) => t,
        Err(e) => return Some(format!("could not load task {task_id}: {e:#}")),
    };
    let run_state = task.runs.last().and_then(|id| RunState::load(id).ok());
    let run_finished = run_state
        .as_ref()
        .is_some_and(|s| s.status.done() || s.parked || s.driver_exited);
    let settled = matches!(
        task.status,
        TaskStatus::Done
            | TaskStatus::Failed
            | TaskStatus::Held
            | TaskStatus::Blocked
            | TaskStatus::Parked
    ) || !task.passes.is_empty()
        || task.attempts > 1
        || run_finished;
    if !settled {
        return None;
    }
    Some(format_task_diagnostic(&task, run_state.as_ref()))
}

/// Format a diagnostic if `task_id` settled into any non-`Done` state.
///
/// Used when waiting for a released task to reach `TaskStatus::Done`: if the
/// task instead lands on `Failed`, `Held`, `Blocked`, or `Parked` (or burns an
/// attempt and retries), the wait fails immediately with full task and run
/// state instead of hanging while `daemon::serve_until` stays idle.
pub fn task_failed(queue: &Queue, task_id: &str) -> Option<String> {
    let task = match queue.get(task_id) {
        Ok(t) => t,
        Err(e) => return Some(format!("could not load task {task_id}: {e:#}")),
    };
    if task.status == TaskStatus::Done {
        return None;
    }
    let failed = matches!(
        task.status,
        TaskStatus::Failed | TaskStatus::Held | TaskStatus::Blocked | TaskStatus::Parked
    ) || !task.passes.is_empty()
        || task.attempts > 1;
    if !failed {
        return None;
    }
    let run_state = task.runs.last().and_then(|id| RunState::load(id).ok());
    Some(format_task_diagnostic(&task, run_state.as_ref()))
}

fn format_task_diagnostic(task: &Task, run_state: Option<&RunState>) -> String {
    let run_summary = match run_state {
        Some(s) => format!(
            "run={} status={:?} parked={} driver_exited={} candidates={:?} last_event={:?}",
            s.id,
            s.status,
            s.parked,
            s.driver_exited,
            s.candidates,
            s.events.last()
        ),
        None => "run=<none>".to_owned(),
    };
    format!(
        "task {} settled with status={:?} attempts={} last_error={:?} hold_reason={:?} block_reason={:?} park_reason={:?} ({run_summary})",
        task.id,
        task.status,
        task.attempts,
        task.last_error,
        task.hold_reason,
        task.block_reason,
        task.park_reason,
    )
}
