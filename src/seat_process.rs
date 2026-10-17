//! A cancelled invocation must leave a cleanup owner for both child and lease.
use crate::repo_lock::SeatLease;
use std::sync::{Arc, Mutex};
use tokio::process::Child;

pub(crate) struct Running {
    child: Option<Child>,
    lease: Option<SeatLease>,
    group: Option<u32>,
}

impl Running {
    pub(crate) fn new(child: Child, lease: Option<SeatLease>) -> Self {
        let group = lease.as_ref().and(child.id());
        Self {
            child: Some(child),
            lease,
            group,
        }
    }

    pub(crate) fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("running seat has a child")
    }

    pub(crate) fn coordinated(&self) -> bool {
        self.lease.is_some()
    }

    pub(crate) async fn finish(&mut self) {
        if let Some(lease) = self.lease.take() {
            let child = self.child.take().expect("running seat has a child");
            if cleanup(child, lease, self.group).await.is_err() {
                // Cleanup could not start. Its fallback owner retains the
                // reservation; expose a resource hold rather than completion.
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        let Some(child) = self.child.take() else {
            return;
        };
        let group = self.group;
        // The receiver may be dropped; the cleanup owner is independent of the
        // caller's future and even its async runtime. It is never abortable.
        drop(cleanup(child, lease, group));
    }
}

fn cleanup(
    child: Child,
    lease: SeatLease,
    group: Option<u32>,
) -> tokio::sync::oneshot::Receiver<()> {
    let (done, receiver) = tokio::sync::oneshot::channel();
    // Retain a fallback owner if the OS cannot start a cleanup thread. Losing
    // cleanup capacity must hold coordination, never unlock a running process.
    let state = Arc::new(Mutex::new(Some((child, lease))));
    let owned = Arc::clone(&state);
    let started = std::thread::Builder::new()
        .name("repository-seat-cleanup".into())
        .spawn(move || {
            let Some((mut child, lease)) = owned.lock().ok().and_then(|mut s| s.take()) else {
                return;
            };
            stop(&mut child, group);
            drop(lease);
            let _ = done.send(());
        });
    if let Err(e) = started {
        tracing::error!("cannot start repository seat cleanup; retaining coordination: {e}");
        std::mem::forget(state);
    }
    receiver
}

fn stop(child: &mut Child, group: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = group {
        use std::collections::{BTreeMap, BTreeSet};
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        // Stop the leader/group before discovering tool children. Terminals
        // can give tools their own process groups, so killpg alone is not a
        // process-tree cleanup. Freeze each discovered child before repeating
        // the snapshot, so it cannot keep creating more children behind us.
        // SAFETY: the child was created as leader of its own process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGSTOP);
        }
        let mut descendants = BTreeMap::new();
        descendants.insert(pid, None);
        let mut groups = BTreeSet::from([pid]);
        loop {
            let mut system = System::new();
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            if system.process(Pid::from_u32(std::process::id())).is_none() {
                // A failed process-table read is unknown. Keep coordination
                // and retry; never infer that the tool tree is absent.
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            let mut discovered = false;
            for (child_pid, process) in system.processes() {
                let id = child_pid.as_u32();
                if process
                    .parent()
                    .is_some_and(|p| descendants.contains_key(&p.as_u32()))
                    && !descendants.contains_key(&id)
                {
                    descendants.insert(id, Some(process.start_time()));
                    // SAFETY: this is a discovered descendant of our stopped
                    // child, not the launcher or an unrelated process group.
                    unsafe {
                        libc::kill(id as i32, libc::SIGSTOP);
                        if libc::getpgid(id as i32) == id as i32 {
                            // Stop/kill the entire tool group too, covering
                            // children forked just before SIGSTOP takes effect.
                            libc::kill(-(id as i32), libc::SIGSTOP);
                            groups.insert(id);
                        }
                    }
                    discovered = true;
                }
            }
            if !discovered {
                break;
            }
        }
        loop {
            let mut system = System::new();
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            if system.process(Pid::from_u32(std::process::id())).is_none() {
                std::thread::sleep(std::time::Duration::from_millis(20));
                continue;
            }
            descendants.retain(|id, start| {
                *id != pid
                    && system
                        .process(Pid::from_u32(*id))
                        .is_some_and(|p| Some(p.start_time()) == *start)
            });
            groups.retain(|group| {
                // SAFETY: these are our CLI/tool groups discovered above.
                unsafe {
                    libc::kill(-(*group as i32), libc::SIGKILL);
                }
                // SAFETY: signal zero only probes the recorded group.
                !(unsafe { libc::kill(-(*group as i32), 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH))
            });
            for id in descendants.keys().filter(|id| **id != pid) {
                // SAFETY: these are the frozen tool descendants discovered
                // above, including tools that created a separate group.
                unsafe {
                    libc::kill(*id as i32, libc::SIGKILL);
                }
            }
            let _ = child.try_wait(); // reap the leader, too
            // A separate tool group must terminate too. Process-table identity
            // checks discard vanished/reused descendant PIDs before signalling.
            if groups.is_empty() && descendants.is_empty() {
                return;
            }
            // Unknown/still alive is a hold, never permission to unlock. This
            // also covers a descendant still being reaped by the OS.
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    #[cfg(not(unix))]
    let _ = group;
    let _ = child.start_kill();
}
