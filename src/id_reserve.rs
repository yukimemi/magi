//! Shared candidate supply and bounded reservation for on-disk identities.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};

const ATTEMPTS: usize = 256;

#[derive(Clone)]
pub(crate) struct IdSource {
    source: Arc<dyn Fn() -> String + Send + Sync>,
    custom: bool,
}

impl std::fmt::Debug for IdSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.custom {
            "IdSource(custom)"
        } else {
            "IdSource"
        })
    }
}

impl IdSource {
    pub(crate) fn new(source: fn() -> String) -> Self {
        Self {
            source: Arc::new(source),
            custom: false,
        }
    }

    pub(crate) fn custom(source: impl Fn() -> String + Send + Sync + 'static) -> Self {
        Self {
            source: Arc::new(source),
            custom: true,
        }
    }

    pub(crate) fn is_custom(&self) -> bool {
        self.custom
    }

    /// `claim` must reserve exclusively before returning true. False means
    /// taken; an uncertain I/O result must propagate as an error. An optional
    /// initial candidate preserves identities already held by attachments.
    pub(crate) fn reserve(
        &self,
        initial: Option<&str>,
        kind: &str,
        mut claim: impl FnMut(&str) -> Result<bool>,
    ) -> Result<String> {
        for attempt in 0..ATTEMPTS {
            let id = match (attempt, initial) {
                (0, Some(id)) => id.to_owned(),
                _ => (self.source)(),
            };
            if claim(&id)? {
                return Ok(id);
            }
        }
        bail!("no free {kind} id after {ATTEMPTS} attempts")
    }
}

/// Exclusively reserve a directory; only AlreadyExists means taken.
pub(crate) fn create_dir(path: &Path) -> Result<bool> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reserve {}", path.display())),
    }
}

/// A broken symlink is taken too. Only a definite NotFound is free.
pub(crate) fn path_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
    }
}

#[cfg(test)]
pub(crate) fn sequence(ids: &[&str]) -> IdSource {
    let ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    IdSource::custom(move || ids[next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_is_bounded_and_io_errors_are_not_retried() {
        let ids = IdSource::custom(|| "taken".into());
        let mut attempts = 0;
        let err = ids
            .reserve(None, "test", |_| {
                attempts += 1;
                Ok(false)
            })
            .unwrap_err();
        assert_eq!(attempts, ATTEMPTS);
        assert!(err.to_string().contains("no free test id"));
        attempts = 0;
        let err = ids
            .reserve(None, "test", |_| {
                attempts += 1;
                bail!("I/O failure")
            })
            .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(err.to_string(), "I/O failure");
    }
}
