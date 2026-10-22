//! Exclusive lock publication, including Windows' delete-pending hand-off.
//! Only the exclusive operation belongs here, never writing the lock body or
//! deciding whether an existing owner is stale.
use std::{
    fs::{File, OpenOptions},
    io,
    path::Path,
    time::{Duration, Instant},
};

const HANDOFF: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(15);

/// Construct once per acquisition and pass through stale recovery and fallback.
/// Preserve an explicit caller deadline across ordinary contention; only an
/// acquisition without one uses the short hand-off budget.
/// A real `AlreadyExists` is returned immediately for the caller to interpret.
#[derive(Clone, Copy)]
pub(crate) struct Retry {
    deadline: Instant,
    permission_denied: bool,
}

impl Retry {
    pub(crate) fn new(deadline: Option<Instant>) -> Self {
        Self {
            deadline: deadline.unwrap_or_else(|| Instant::now() + HANDOFF),
            permission_denied: cfg!(windows),
        }
    }

    /// Task writes historically retried PermissionDenied on every platform,
    /// for their whole existing wait budget. Preserve that non-Windows policy.
    pub(crate) fn task_write(deadline: Instant) -> Self {
        if cfg!(windows) {
            Self::new(Some(deadline))
        } else {
            Self {
                deadline,
                permission_denied: true,
            }
        }
    }

    pub(crate) fn create(self, path: &Path) -> io::Result<File> {
        self.run(|| OpenOptions::new().write(true).create_new(true).open(path))
    }

    pub(crate) fn run<T>(self, operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
        self.run_with(operation, Instant::now, std::thread::sleep)
    }

    fn pause(self, error: &io::Error, now: Instant) -> Option<Duration> {
        if !self.permission_denied || error.kind() != io::ErrorKind::PermissionDenied {
            return None;
        }
        let remaining = self.deadline.saturating_duration_since(now);
        (!remaining.is_zero()).then_some(POLL.min(remaining))
    }

    /// Check cancellation before and after each delete-pending wait.
    pub(crate) async fn run_async_unless<T>(
        self,
        operation: impl FnMut() -> io::Result<T>,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<Option<T>> {
        self.run_async_with(operation, cancel, Instant::now, tokio::time::sleep)
            .await
    }

    async fn run_async_with<T, F: std::future::Future<Output = ()>>(
        self,
        mut operation: impl FnMut() -> io::Result<T>,
        cancel: &(impl Fn() -> bool + ?Sized),
        now: impl Fn() -> Instant,
        mut sleep: impl FnMut(Duration) -> F,
    ) -> io::Result<Option<T>> {
        loop {
            match operation() {
                Err(error) => {
                    let Some(delay) = self.pause(&error, now()) else {
                        return Err(error);
                    };
                    if cancel() {
                        return Ok(None);
                    }
                    sleep(delay).await;
                    if cancel() {
                        return Ok(None);
                    }
                    if now() >= self.deadline {
                        return Err(error);
                    }
                }
                Ok(value) => return Ok(Some(value)),
            }
        }
    }

    fn run_with<T>(
        self,
        mut operation: impl FnMut() -> io::Result<T>,
        now: impl Fn() -> Instant,
        mut sleep: impl FnMut(Duration),
    ) -> io::Result<T> {
        loop {
            match operation() {
                Err(e) => {
                    let Some(delay) = self.pause(&e, now()) else {
                        return Err(e);
                    };
                    sleep(delay);
                    // Do not acquire after expiry or discard the actual OS error.
                    if now() >= self.deadline {
                        return Err(e);
                    }
                }
                result => return result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sequence(
        kinds: &[Option<io::ErrorKind>],
        retry: bool,
        budget: Duration,
    ) -> (io::Result<()>, usize, Duration) {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut calls = 0;
        let result = Retry {
            deadline: start + budget,
            permission_denied: retry,
        }
        .run_with(
            || {
                let kind = kinds[calls.min(kinds.len() - 1)];
                calls += 1;
                kind.map_or(Ok(()), |kind| Err(io::Error::new(kind, "original error")))
            },
            || start + elapsed.get(),
            |wait| elapsed.set(elapsed.get() + wait),
        );
        (result, calls, elapsed.get())
    }

    #[tokio::test]
    async fn async_retry_checks_cancellation_before_sleep() {
        let start = Instant::now();
        let retry = Retry {
            deadline: start + HANDOFF,
            permission_denied: true,
        };
        let mut calls = 0;
        let result = retry
            .run_async_with(
                || {
                    calls += 1;
                    Err::<(), _>(io::ErrorKind::PermissionDenied.into())
                },
                &|| true,
                || start,
                |_| async { panic!("cancelled acquisition must not sleep") },
            )
            .await
            .unwrap();
        assert!(result.is_none());
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn async_retry_checks_cancellation_after_sleep() {
        let start = Instant::now();
        let cancelled = Cell::new(false);
        let mut calls = 0;
        let mut sleeps = 0;
        let result = Retry {
            deadline: start + HANDOFF,
            permission_denied: true,
        }
        .run_async_with(
            || {
                calls += 1;
                if calls == 1 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            },
            &|| cancelled.get(),
            || start,
            |_| {
                sleeps += 1;
                cancelled.set(true);
                std::future::ready(())
            },
        )
        .await
        .unwrap();
        assert!(result.is_none());
        assert_eq!(calls, 1, "must not acquire after cancellation");
        assert_eq!(sleeps, 1);
    }

    #[tokio::test]
    async fn async_retry_preserves_success_conflict_and_expiry() {
        for terminal in [None, Some(io::ErrorKind::AlreadyExists)] {
            for budget in [HANDOFF, Duration::from_millis(7)] {
                let start = Instant::now();
                let elapsed = Cell::new(Duration::ZERO);
                let mut calls = 0;
                let result = Retry {
                    deadline: start + budget,
                    permission_denied: true,
                }
                .run_async_with(
                    || {
                        calls += 1;
                        let kind = if calls == 1 {
                            Some(io::ErrorKind::PermissionDenied)
                        } else {
                            terminal
                        };
                        kind.map_or(Ok(()), |kind| Err(kind.into()))
                    },
                    &|| false,
                    || start + elapsed.get(),
                    |delay| {
                        elapsed.set(elapsed.get() + delay);
                        std::future::ready(())
                    },
                )
                .await;
                if budget < POLL {
                    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
                    assert_eq!(calls, 1);
                    assert_eq!(elapsed.get(), budget);
                } else {
                    match terminal {
                        None => assert_eq!(result.unwrap(), Some(())),
                        Some(kind) => assert_eq!(result.unwrap_err().kind(), kind),
                    }
                    assert_eq!(calls, 2);
                }
            }
        }
    }

    #[test]
    fn delete_pending_then_success() {
        let (result, calls, _) = sequence(
            &[Some(io::ErrorKind::PermissionDenied), None],
            true,
            HANDOFF,
        );
        assert!(result.is_ok());
        assert_eq!(calls, 2);
    }

    #[test]
    fn real_conflict_is_returned_immediately() {
        for kinds in [
            vec![Some(io::ErrorKind::AlreadyExists)],
            vec![
                Some(io::ErrorKind::PermissionDenied),
                Some(io::ErrorKind::AlreadyExists),
            ],
        ] {
            let (result, calls, _) = sequence(&kinds, true, HANDOFF);
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(calls, kinds.len());
        }
    }

    #[test]
    fn expiry_preserves_error_and_never_extends_budget() {
        for budget in [Duration::ZERO, Duration::from_millis(7)] {
            let (result, _, elapsed) =
                sequence(&[Some(io::ErrorKind::PermissionDenied)], true, budget);
            let error = result.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(error.to_string(), "original error");
            assert_eq!(elapsed, budget);
        }
    }

    #[test]
    fn non_windows_errors_are_immediate_except_task_write_compatibility() {
        let (result, calls, elapsed) = sequence(
            &[Some(io::ErrorKind::PermissionDenied), None],
            false,
            HANDOFF,
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls, 1);
        assert_eq!(elapsed, Duration::ZERO);
        let deadline = Instant::now() + Duration::from_secs(3);
        let retry = Retry::task_write(deadline);
        assert!(retry.permission_denied);
        assert_eq!(retry.deadline, deadline);
    }

    #[test]
    fn unrelated_errors_are_not_retried() {
        let (result, calls, elapsed) =
            sequence(&[Some(io::ErrorKind::NotFound), None], true, HANDOFF);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(calls, 1);
        assert_eq!(elapsed, Duration::ZERO);
    }

    #[test]
    fn a_reused_budget_does_not_restart_after_conflict_or_fallback() {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let retry = Retry {
            deadline: start + Duration::from_millis(20),
            permission_denied: true,
        };
        for terminal in [io::ErrorKind::AlreadyExists, io::ErrorKind::Unsupported] {
            elapsed.set(Duration::ZERO);
            let mut calls = 0;
            let first = retry.run_with::<()>(
                || {
                    calls += 1;
                    Err(if calls == 1 {
                        io::ErrorKind::PermissionDenied
                    } else {
                        terminal
                    }
                    .into())
                },
                || start + elapsed.get(),
                |d| elapsed.set(elapsed.get() + d),
            );
            assert_eq!(first.unwrap_err().kind(), terminal);
            let next = retry.run_with::<()>(
                || {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "still denied",
                    ))
                },
                || start + elapsed.get(),
                |d| elapsed.set(elapsed.get() + d),
            );
            assert_eq!(next.unwrap_err().to_string(), "still denied");
            assert_eq!(elapsed.get(), Duration::from_millis(20));
        }
    }

    #[test]
    fn delete_pending_after_ordinary_contention_uses_remaining_caller_budget() {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let retry = Retry {
            permission_denied: true,
            ..Retry::new(Some(start + Duration::from_secs(10)))
        };
        let conflict = retry.run_with::<()>(
            || Err(io::ErrorKind::AlreadyExists.into()),
            || start + elapsed.get(),
            |_| panic!("real conflict must return immediately"),
        );
        assert_eq!(conflict.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        elapsed.set(Duration::from_millis(150));
        let mut calls = 0;
        let result = retry.run_with(
            || {
                calls += 1;
                if calls == 1 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            },
            || start + elapsed.get(),
            |delay| elapsed.set(elapsed.get() + delay),
        );
        assert!(result.is_ok());
        assert_eq!(calls, 2);
        assert_eq!(elapsed.get(), Duration::from_millis(150) + POLL);
    }

    #[tokio::test]
    async fn async_delete_pending_after_poll_uses_remaining_caller_budget() {
        let start = Instant::now();
        let budget = Duration::from_secs(10);
        let poll = Duration::from_millis(50);
        let elapsed = Cell::new(Duration::ZERO);
        let retry = Retry {
            permission_denied: true,
            ..Retry::new(Some(start + budget))
        };
        let conflict = retry
            .run_async_with::<(), _>(
                || Err(io::ErrorKind::AlreadyExists.into()),
                &|| false,
                || start + elapsed.get(),
                |_| async { panic!("real conflict must return immediately") },
            )
            .await;
        assert_eq!(conflict.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        elapsed.set(poll * 3);
        let mut calls = 0;
        let result = retry
            .run_async_with(
                || {
                    calls += 1;
                    if calls == 1 {
                        Err(io::ErrorKind::PermissionDenied.into())
                    } else {
                        Ok(())
                    }
                },
                &|| false,
                || start + elapsed.get(),
                |delay| {
                    elapsed.set(elapsed.get() + delay);
                    std::future::ready(())
                },
            )
            .await;
        assert_eq!(result.unwrap(), Some(()));
        assert_eq!(calls, 2);
        assert_eq!(elapsed.get(), poll * 3 + POLL);
        assert_eq!(retry.deadline, start + budget);
    }

    #[test]
    fn caller_deadline_is_preserved_and_only_unspecified_waits_are_capped() {
        let deadline = Instant::now();
        assert_eq!(Retry::new(Some(deadline)).deadline, deadline);
        let before = Instant::now();
        let deadline = before + Duration::from_secs(10);
        assert_eq!(Retry::new(Some(deadline)).deadline, deadline);
        let retry = Retry::new(None);
        assert!(retry.deadline <= Instant::now() + HANDOFF);
        assert!(retry.deadline >= before + HANDOFF);
    }
}
