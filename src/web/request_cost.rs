//! Request-local observation of real config discovery and git invocations.
//! Compiled only into unit tests; no process-wide counter or environment change.
use std::cell::RefCell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct Counts {
    discoveries: AtomicUsize,
    git: AtomicUsize,
}
tokio::task_local! { static REQUEST: Arc<Counts>; }
thread_local! { static BLOCKING: RefCell<Option<Arc<Counts>>> = const { RefCell::new(None) }; }

fn current() -> Option<Arc<Counts>> {
    REQUEST
        .try_with(Arc::clone)
        .ok()
        .or_else(|| BLOCKING.with(|c| c.borrow().clone()))
}
pub(crate) fn discovery() {
    if let Some(c) = current() {
        c.discoveries.fetch_add(1, Ordering::Relaxed);
    }
}
pub(crate) fn git() {
    if let Some(c) = current() {
        c.git.fetch_add(1, Ordering::Relaxed);
    }
}

/// Restore the worker's previous scope even when a closure unwinds.
pub(super) fn carry<T>(job: impl FnOnce() -> T + Send + 'static) -> impl FnOnce() -> T + Send {
    let counts = current();
    move || {
        struct Restore(Option<Arc<Counts>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                BLOCKING.with(|c| {
                    c.replace(self.0.take());
                });
            }
        }
        let _restore = Restore(BLOCKING.with(|c| c.replace(counts)));
        job()
    }
}

pub(super) async fn measure<T>(job: impl std::future::Future<Output = T>) -> (T, (usize, usize)) {
    let counts = Arc::new(Counts::default());
    let result = REQUEST.scope(counts.clone(), job).await;
    (
        result,
        (
            counts.discoveries.load(Ordering::Relaxed),
            counts.git.load(Ordering::Relaxed),
        ),
    )
}

pub(super) async fn observe(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let (mut response, (discoveries, git)) = measure(next.run(req)).await;
    for (header, value) in [("x-test-discoveries", discoveries), ("x-test-git", git)] {
        response
            .headers_mut()
            .insert(header, value.to_string().parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_cost_carries_and_restores_nested_worker_scopes() {
        let outer = Arc::new(Counts::default());
        let inner = Arc::new(Counts::default());
        let job = REQUEST
            .scope(outer.clone(), async {
                let nested = REQUEST
                    .scope(inner.clone(), async {
                        carry(|| {
                            discovery();
                            git();
                            panic!("exercise restoration");
                        })
                    })
                    .await;
                carry(move || {
                    discovery();
                    assert!(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(nested)).is_err()
                    );
                    git();
                })
            })
            .await;
        tokio::task::spawn_blocking(move || {
            job();
            assert!(
                current().is_none(),
                "no observation leaks into the worker pool"
            );
        })
        .await
        .unwrap();
        assert_eq!(outer.discoveries.load(Ordering::Relaxed), 1);
        assert_eq!(outer.git.load(Ordering::Relaxed), 1);
        assert_eq!(inner.discoveries.load(Ordering::Relaxed), 1);
        assert_eq!(inner.git.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn request_cost_parallel_scopes_are_isolated() {
        async fn count(n: usize) -> Arc<Counts> {
            let counts = Arc::new(Counts::default());
            REQUEST
                .scope(counts.clone(), async move {
                    tokio::task::spawn_blocking(carry(move || {
                        for _ in 0..n {
                            discovery();
                            git();
                        }
                    }))
                    .await
                    .unwrap();
                })
                .await;
            counts
        }
        let (a, b) = tokio::join!(count(3), count(19));
        assert_eq!(a.discoveries.load(Ordering::Relaxed), 3);
        assert_eq!(a.git.load(Ordering::Relaxed), 3);
        assert_eq!(b.discoveries.load(Ordering::Relaxed), 19);
        assert_eq!(b.git.load(Ordering::Relaxed), 19);
    }
}
