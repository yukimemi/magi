//! Execution leases, distinct from the lifetime of the process hosting a run.
//!
//! Lock order is public-head owner lock, then execution lease. Never retain
//! the owner lock while executing the graph. Lease metadata is tied to a pass;
//! an unlocked file without matching metadata proves nothing about legacy runs.
use std::{
    fs::File,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::run::{Liveness, RunState};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    pid: Option<u32>,
    started_at: Option<String>,
    pass: Option<jiff::Timestamp>,
    pass_index: Option<usize>,
}

impl Identity {
    fn of(state: &RunState) -> Self {
        Self {
            pid: state.driver_pid,
            started_at: state.driver_started_at.clone(),
            pass: state.passes.last().map(|p| p.started_at),
            pass_index: state.passes.last().map(|p| p.index),
        }
    }
}

/// Release ownership when the guard ends, even if a concurrent fork still
/// holds a copy of the file description before exec closes it.
struct ExecutionLease {
    _file: File,
}

#[cfg(unix)]
impl Drop for ExecutionLease {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the live file owns this descriptor. Explicit unlock releases
        // the flock even while an inherited descriptor remains open.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn lease(home: &Path, id: &str) -> Result<ExecutionLease> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("runs").join(id).join("driver.lock"))?;
    crate::pr_head::try_lock_file(&file).context("run execution lease unavailable")?;
    Ok(ExecutionLease { _file: file })
}

/// A held lease is evidence of an execution, including within a long-lived
/// daemon. A free lease proves death only when its metadata names this pass.
/// The caller supplies the same process judge used for legacy records.
pub(crate) fn judge(state: &RunState, home: &Path, fallback: Liveness) -> Liveness {
    if state.driver_exited {
        // A daemon claim always wins, even in the gap after graph completion.
        return fallback;
    }
    let dir = home.join("runs").join(&state.id);
    let Ok(body) = std::fs::read(dir.join("driver.json")) else {
        return fallback;
    };
    let Ok(recorded) = serde_json::from_slice::<Identity>(&body) else {
        return Liveness::Unknown;
    };
    if recorded != Identity::of(state) || recorded.pass.is_none() {
        return fallback;
    }
    match lease(home, &state.id) {
        Ok(_) => Liveness::Dead,
        Err(e)
            if e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                e.kind() == std::io::ErrorKind::WouldBlock
                    || (cfg!(windows) && e.raw_os_error() == Some(33))
            }) =>
        {
            Liveness::Live
        }
        Err(_) => Liveness::Unknown,
    }
}

/// Owns only the execution lease, so cancellation and unwinding also release
/// ownership. The pass identity prevents a stale cleanup erasing a successor.
pub(crate) struct DriverGuard {
    home: PathBuf,
    id: String,
    identity: Identity,
    _lease: ExecutionLease,
    finished: bool,
}

impl DriverGuard {
    /// Caller holds the short owner lock. Acquire before changing run.json.
    pub(crate) fn acquire(home: &Path, state: &RunState) -> Result<Self> {
        let held = lease(home, &state.id)?;
        Ok(Self {
            home: home.to_owned(),
            id: state.id.clone(),
            identity: Identity::of(state),
            _lease: held,
            finished: false,
        })
    }

    /// Bind cleanup before saving a new claim. Preserve the previous lease
    /// metadata until run.json commits, so a failed claim save cannot erase
    /// the evidence for the preceding stopped pass.
    pub(crate) fn bind(&mut self, state: &RunState) {
        self.identity = Identity::of(state);
    }

    pub(crate) fn record(&mut self, state: &RunState) -> Result<()> {
        self.bind(state);
        let dir = self.home.join("runs").join(&self.id);
        let tmp = dir.join("driver.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&self.identity)?)?;
        std::fs::rename(tmp, dir.join("driver.json"))?;
        Ok(())
    }

    /// Keep the caller's retained state from resurrecting an exited claim
    /// after its execution future is dropped. Disk cleanup still checks the
    /// independently loaded generation in Drop.
    pub(crate) fn release_local(&self, state: &mut RunState) {
        if Identity::of(state) == self.identity {
            clear(state);
        }
    }

    /// Save the final graph state and its released ownership together.
    pub(crate) fn finish(&mut self, state: &mut RunState) -> Result<()> {
        let _owner = crate::pr_head::owner_lock(&self.home, &self.id)?;
        let disk = RunState::load_under(&self.id, &self.home)?;
        if Identity::of(&disk) != self.identity || disk.released_to.is_some() {
            bail!("run driver ownership changed before execution finished");
        }
        clear(state);
        state.save_under(&self.home)?;
        self.finished = true;
        Ok(())
    }
}

fn clear(state: &mut RunState) {
    state.driver_pid = None;
    state.driver_started_at = None;
    state.driver_exited = true;
}

impl Drop for DriverGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let result = (|| -> Result<()> {
            let _owner = crate::pr_head::owner_lock(&self.home, &self.id)?;
            let mut disk = RunState::load_under(&self.id, &self.home)?;
            if Identity::of(&disk) == self.identity && disk.released_to.is_none() {
                clear(&mut disk);
                disk.save_under(&self.home)?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("could not release run {} driver: {e:#}", self.id);
        }
        // On save failure the OS still releases the lease; the next judge or
        // startup reconciliation can prove this pass has stopped.
    }
}

/// Startup recovery preserves status, candidates and checkpoints. Only the
/// ownership evidence is repaired; an interrupted graph remains resumable.
/// Returns the run IDs whose repaired ownership was successfully saved.
pub(crate) fn reconcile(home: &Path) -> Vec<String> {
    let previous = crate::daemon::read_status(home);
    let mut reconciled = Vec::new();
    for id in crate::run::list_ids_in(&home.join("runs")) {
        let Ok(_owner) = crate::pr_head::owner_lock(home, &id) else {
            continue;
        };
        let Ok(mut state) = RunState::load_under(&id, home) else {
            continue;
        };
        let previous_gone = previous.as_ref().is_some_and(|old| {
            old.pid == state.driver_pid
                && state.driver_pid.is_some()
                && state.liveness_with(
                    false,
                    crate::proc::pid_status,
                    crate::proc::process_started_at,
                ) == Liveness::Dead
        });
        let claims =
            !previous_gone && crate::daemon::is_working_on(home, &id, jiff::Timestamp::now());
        if !state.driver_exited && state.liveness_under(home, claims) == Liveness::Dead {
            clear(&mut state);
            state.clear_active();
            if let Err(e) = state.save_under(home) {
                tracing::warn!("could not reconcile run {id} driver: {e:#}");
            } else {
                reconciled.push(id);
            }
        }
    }
    reconciled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, run::RunStatus};

    fn state(home: &Path) -> RunState {
        let mut state = RunState::new(
            PathBuf::from("repo"),
            "main".into(),
            "base".into(),
            "task".into(),
            Config::default(),
        );
        state.driver_pid = Some(std::process::id());
        state.driver_started_at = crate::proc::process_started_at(std::process::id());
        state.begin_pass();
        state.save_under(home).unwrap();
        state
    }

    fn claim(home: &Path, state: &RunState) -> DriverGuard {
        let mut guard = DriverGuard::acquire(home, state).unwrap();
        guard.record(state).unwrap();
        guard
    }

    #[test]
    fn every_graph_outcome_releases_ownership_with_its_final_state() {
        for status in [
            RunStatus::Gating,
            RunStatus::Blocked,
            RunStatus::Stalled,
            RunStatus::Failed,
            RunStatus::Ready,
            RunStatus::Merged,
            RunStatus::Landing,
        ] {
            for parked in [false, true] {
                let home = tempfile::tempdir().unwrap();
                let mut s = state(home.path());
                let mut guard = claim(home.path(), &s);
                assert_eq!(s.liveness_under(home.path(), false), Liveness::Live);
                assert!(DriverGuard::acquire(home.path(), &s).is_err());
                s.status = status;
                s.parked = parked;
                guard.finish(&mut s).unwrap();
                let disk = RunState::load_under(&s.id, home.path()).unwrap();
                assert_eq!(disk.status, status);
                assert_eq!(disk.parked, parked);
                assert!(disk.driver_exited);
                assert_eq!(disk.driver_pid, None);
                assert_eq!(disk.liveness_under(home.path(), false), Liveness::Dead);
            }
        }
    }

    #[test]
    fn error_and_unwind_release_the_driver() {
        let home = tempfile::tempdir().unwrap();
        let s = state(home.path());
        let result = (|| -> Result<()> {
            let _guard = claim(home.path(), &s);
            bail!("postprocessing failed")
        })();
        assert!(result.is_err());
        assert!(
            RunState::load_under(&s.id, home.path())
                .unwrap()
                .driver_exited
        );
        let mut s = state(home.path());
        s.driver_exited = false;
        s.save_under(home.path()).unwrap();
        let _ = std::panic::catch_unwind(|| {
            let _guard = claim(home.path(), &s);
            panic!("graph unwound");
        });
        assert!(
            RunState::load_under(&s.id, home.path())
                .unwrap()
                .driver_exited
        );
    }

    #[tokio::test]
    async fn dropping_an_execution_future_releases_its_driver() {
        let home = tempfile::tempdir().unwrap();
        let s = state(home.path());
        let mut future = Box::pin(async {
            let _guard = claim(home.path(), &s);
            std::future::pending::<()>().await;
        });
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(future.as_mut(), &mut cx).is_pending());
        drop(future);
        assert!(
            RunState::load_under(&s.id, home.path())
                .unwrap()
                .driver_exited
        );
    }

    #[test]
    fn a_free_matching_lease_recovers_even_while_the_host_process_lives() {
        let home = tempfile::tempdir().unwrap();
        let mut s = state(home.path());
        let guard = claim(home.path(), &s);
        // Simulate a failed Drop save: ownership metadata survives but the OS
        // lease is released. No private build or subprocess is needed.
        let path = home.path().join("runs").join(&s.id).join("run.json");
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        drop(guard);
        std::fs::write(&path, saved).unwrap();
        assert_eq!(s.liveness_under(home.path(), false), Liveness::Dead);
        assert_eq!(s.liveness_under(home.path(), true), Liveness::Live);
        s.status = RunStatus::Gating;
        s.save_under(home.path()).unwrap();
        assert_eq!(reconcile(home.path()), vec![s.id.clone()]);
        let recovered = RunState::load_under(&s.id, home.path()).unwrap();
        assert!(recovered.driver_exited);
        assert_eq!(recovered.driver_pid, None);
        assert_eq!(recovered.status, RunStatus::Gating);
        assert!(recovered.status.resumable());
        assert!(reconcile(home.path()).is_empty());
    }

    #[test]
    fn restart_preserves_unknown_and_live_drivers_and_reconciles_reused_pids() {
        let home = tempfile::tempdir().unwrap();
        let mut stale = state(home.path());
        stale.driver_started_at = Some("1".into());
        stale.save_under(home.path()).unwrap();
        let mut previous = crate::daemon::Status::new();
        previous.current.push(crate::daemon::Current {
            task: "task".into(),
            run: stale.id.clone(),
        });
        crate::daemon::write_status_to(&home.path().join("daemon.json"), &previous).unwrap();
        let mut unknown = state(home.path());
        unknown.driver_started_at = None;
        unknown.save_under(home.path()).unwrap();
        let live = state(home.path());
        let _guard = claim(home.path(), &live);
        assert_eq!(reconcile(home.path()), vec![stale.id.clone()]);
        let stale = RunState::load_under(&stale.id, home.path()).unwrap();
        assert!(stale.driver_exited);
        assert!(stale.status.resumable());
        assert!(
            !RunState::load_under(&unknown.id, home.path())
                .unwrap()
                .driver_exited
        );
        assert!(
            !RunState::load_under(&live.id, home.path())
                .unwrap()
                .driver_exited
        );
    }

    #[test]
    fn a_failed_claim_save_preserves_proof_for_the_previous_pass() {
        let home = tempfile::tempdir().unwrap();
        let mut s = state(home.path());
        let old = claim(home.path(), &s);
        // Model descriptors inherited by concurrent process launches before exec.
        #[cfg(unix)]
        let _old_inherited = old._lease._file.try_clone().unwrap();
        let path = home.path().join("runs").join(&s.id).join("run.json");
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        drop(old);
        std::fs::write(&path, saved).unwrap();
        let mut new = DriverGuard::acquire(home.path(), &s).unwrap();
        #[cfg(unix)]
        let _new_inherited = new._lease._file.try_clone().unwrap();
        s.begin_pass();
        new.bind(&s);
        // A failed run.json write leaves the old disk generation untouched.
        drop(new);
        let disk = RunState::load_under(&s.id, home.path()).unwrap();
        assert!(!disk.driver_exited);
        assert_eq!(disk.liveness_under(home.path(), false), Liveness::Dead);
    }

    #[test]
    #[cfg(unix)]
    fn execution_lease_releases_even_with_an_inherited_descriptor() {
        for finished in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let mut s = state(home.path());
            let mut guard = claim(home.path(), &s);
            let inherited = guard._lease._file.try_clone().unwrap();
            assert!(DriverGuard::acquire(home.path(), &s).is_err());
            if finished {
                guard.finish(&mut s).unwrap();
            }
            drop(guard);
            let next = DriverGuard::acquire(home.path(), &s)
                .expect("ending execution must release its lease before inherited copies close");
            drop(inherited);
            assert!(
                DriverGuard::acquire(home.path(), &s).is_err(),
                "closing an old copy must not unlock the next execution"
            );
            drop(next);
            assert!(DriverGuard::acquire(home.path(), &s).is_ok());
        }
    }

    #[test]
    fn a_free_lease_is_not_proof_for_a_legacy_record() {
        let home = tempfile::tempdir().unwrap();
        let mut s = state(home.path());
        s.driver_started_at = None;
        s.save_under(home.path()).unwrap();
        drop(lease(home.path(), &s.id).unwrap());
        assert_eq!(s.liveness_under(home.path(), false), Liveness::Unknown);
        assert!(reconcile(home.path()).is_empty());
        assert!(
            !RunState::load_under(&s.id, home.path())
                .unwrap()
                .driver_exited
        );
    }

    #[test]
    fn stale_guard_and_old_lease_metadata_cannot_release_a_new_generation() {
        let home = tempfile::tempdir().unwrap();
        let mut s = state(home.path());
        let guard = claim(home.path(), &s);
        s.begin_pass();
        s.save_under(home.path()).unwrap();
        drop(guard);
        let disk = RunState::load_under(&s.id, home.path()).unwrap();
        assert!(!disk.driver_exited);
        assert_ne!(disk.liveness_under(home.path(), false), Liveness::Dead);
    }
}
