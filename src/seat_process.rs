//! A cancelled invocation must leave a cleanup owner for both child and lease.
use crate::repo_lock::SeatLease;
use std::sync::{Arc, Mutex};
use tokio::process::Child;

pub(crate) struct Running {
    child: Option<Child>,
    lease: Option<SeatLease>,
    group: Option<u32>,
    outputs: Vec<crate::retention::OutputLease>,
    witness: Option<std::fs::File>,
    owner: Option<std::ffi::OsString>,
}

impl Running {
    fn new(child: Child, lease: Option<SeatLease>) -> Self {
        let group = lease.as_ref().and(child.id());
        Self {
            child: Some(child),
            lease,
            group,
            outputs: Vec::new(),
            witness: None,
            owner: None,
        }
    }

    pub(crate) fn spawn(
        mut command: tokio::process::Command,
        lease: Option<SeatLease>,
        outputs: Vec<crate::retention::OutputLease>,
    ) -> std::io::Result<Self> {
        #[cfg(unix)]
        let mut witness = None;
        #[cfg(not(unix))]
        let witness = None;
        let owner = (lease.is_some() || !outputs.is_empty()).then(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT_OWNER: AtomicU64 = AtomicU64::new(0);
            let token = format!(
                "{}-{}-{}",
                std::process::id(),
                NEXT_OWNER.fetch_add(1, Ordering::Relaxed),
                crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4()
            );
            command.env("MAGI_PROCESS_OWNER", &token);
            std::ffi::OsString::from(format!("MAGI_PROCESS_OWNER={token}"))
        });
        #[cfg(unix)]
        let _writer = if lease.is_some() || !outputs.is_empty() {
            use std::os::fd::{AsRawFd, OwnedFd};
            // Create before spawning: even a tool that forks, setsid()s and
            // outlives its CLI without ever appearing in a process snapshot
            // inherits a writer. EOF is independent of parent/group identity.
            let (reader, writer) = std::os::unix::net::UnixStream::pair()?;
            reader.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
            witness = Some(std::fs::File::from(OwnedFd::from(reader)));
            let fd = writer.as_raw_fd();
            let output_fds: Vec<_> = outputs.iter().map(|lease| lease.descriptor()).collect();
            command.process_group(0);
            // SAFETY: only async-signal-safe fcntl runs after fork. Clear
            // CLOEXEC only in this child, never in the multithreaded launcher.
            // `writer` stays alive until spawn returns and command is dropped.
            unsafe {
                command.pre_exec(move || {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Tools also hold the actual output reservations. If the
                    // launcher dies, its cleanup thread cannot keep a lease;
                    // these inherited descriptors still exclude reclamation.
                    for output in &output_fds {
                        if libc::fcntl(*output, libc::F_SETFD, 0) == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            Some(writer)
        } else {
            None
        };
        let child = command.spawn()?;
        let mut running = Self::new(child, lease);
        if !outputs.is_empty() {
            running.group = running.child.as_ref().and_then(Child::id);
        }
        running.outputs = outputs;
        running.witness = witness;
        running.owner = owner;
        Ok(running)
    }

    pub(crate) fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("running seat has a child")
    }

    pub(crate) fn coordinated(&self) -> bool {
        self.lease.is_some() || !self.outputs.is_empty()
    }

    pub(crate) async fn finish(&mut self) {
        if self.coordinated() {
            let lease = self.lease.take();
            let outputs = std::mem::take(&mut self.outputs);
            let child = self.child.take().expect("running seat has a child");
            if cleanup(
                child,
                lease,
                outputs,
                self.group,
                self.witness.take(),
                self.owner.take(),
            )
            .await
            .is_err()
            {
                // Cleanup could not start. Its fallback owner retains the
                // reservation; expose a resource hold rather than completion.
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.coordinated() {
            return;
        }
        let lease = self.lease.take();
        let outputs = std::mem::take(&mut self.outputs);
        let Some(child) = self.child.take() else {
            return;
        };
        let group = self.group;
        // The receiver may be dropped; the cleanup owner is independent of the
        // caller's future and even its async runtime. It is never abortable.
        drop(cleanup(
            child,
            lease,
            outputs,
            group,
            self.witness.take(),
            self.owner.take(),
        ));
    }
}

fn cleanup(
    child: Child,
    lease: Option<SeatLease>,
    outputs: Vec<crate::retention::OutputLease>,
    group: Option<u32>,
    witness: Option<std::fs::File>,
    owner: Option<std::ffi::OsString>,
) -> tokio::sync::oneshot::Receiver<()> {
    let (done, receiver) = tokio::sync::oneshot::channel();
    // Retain a fallback owner if the OS cannot start a cleanup thread. Losing
    // cleanup capacity must hold coordination, never unlock a running process.
    let state = Arc::new(Mutex::new(Some((child, lease, outputs, witness))));
    let owned = Arc::clone(&state);
    let started = std::thread::Builder::new()
        .name("repository-seat-cleanup".into())
        .spawn(move || {
            let Some((mut child, lease, outputs, witness)) =
                owned.lock().ok().and_then(|mut s| s.take())
            else {
                return;
            };
            stop(&mut child, group, owner.as_deref());
            if let Some(mut witness) = witness {
                use std::io::Read;
                // A reparented tool may have escaped discovery above. Keep
                // every lease until the last inherited writer closes. This
                // can wait for a tool to finish naturally; it must not grant
                // destructive maintenance permission just because its CLI died.
                tracing::debug!("waiting for seat tool liveness witness before releasing leases");
                let mut byte = [0];
                let mut reported = false;
                loop {
                    match witness.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            if !reported {
                                tracing::warn!(
                                    "seat tool still owns liveness witness; retaining repository and output leases"
                                );
                                reported = true;
                            }
                        }
                        Err(e) => {
                            tracing::error!("seat tool liveness unknown; retaining leases: {e}");
                            loop {
                                std::thread::park();
                            }
                        }
                    }
                }
            }
            drop(lease);
            drop(outputs);
            let _ = done.send(());
        });
    if let Err(e) = started {
        tracing::error!("cannot start repository seat cleanup; retaining coordination: {e}");
        std::mem::forget(state);
    }
    receiver
}

fn stop(child: &mut Child, group: Option<u32>, owner: Option<&std::ffi::OsStr>) {
    #[cfg(unix)]
    if let Some(pid) = group {
        use std::collections::{BTreeMap, BTreeSet};
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
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
                ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
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
                // The inherited owner token survives reparenting and exec,
                // including ordinary tool launchers that close extra FDs.
                // An unrelated process never receives this per-spawn token.
                if (process
                    .parent()
                    .is_some_and(|p| descendants.contains_key(&p.as_u32()))
                    || owner.is_some_and(|tag| process.environ().iter().any(|e| e == tag)))
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
    let _ = (group, owner);
    let _ = child.start_kill();
}

#[cfg(all(test, unix))]
mod tests {
    use crate::proc::Quiet as _;

    /// Exit without destructors to model loss of the launcher and its cleanup
    /// thread while a tool still owns output descriptors.
    #[test]
    fn crashed_launcher_helper() {
        let Some(root) = std::env::var_os("MAGI_TEST_CRASHED_LAUNCHER") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let worktree = std::path::PathBuf::from(std::env::var_os("MAGI_TEST_WORKTREE").unwrap());
        let outputs = vec![
            crate::retention::OutputLease::borrow(&worktree)
                .unwrap()
                .unwrap(),
            crate::retention::OutputLease::cache(&root, &root.join("cache"), false).unwrap(),
        ];
        let pid_file = root.join("tool.pid");
        let mut command = tokio::process::Command::new("sh");
        command
            .args(["-c", "echo $$ > \"$MAGI_TEST_TOOL_PID\"; exec sleep 10"])
            .env("MAGI_TEST_TOOL_PID", &pid_file);
        // A runtime is required by tokio's process registration even though
        // the launcher deliberately never polls or drops the running process.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _entered = runtime.enter();
        let _running = super::Running::spawn(command, None, outputs).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pid_file.exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::process::exit(0);
    }

    /// Executed in a separate copy of the test binary. No interpreter or
    /// compiler dependency: reproduce fork/setsid/reparent without source edits.
    #[test]
    fn orphan_tool_helper() {
        use std::os::fd::AsRawFd;
        let Some(path) = std::env::var_os("MAGI_TEST_ORPHAN_PID") else {
            return;
        };
        if std::env::var_os("MAGI_TEST_CLEAR_OWNER").is_some()
            && std::env::var_os("MAGI_PROCESS_OWNER").is_some()
        {
            use std::os::unix::process::CommandExt;
            // Clear the initial environment too (not just libc's environ),
            // so kernel process inspection cannot recover the owner token.
            panic!(
                "helper exec failed: {}",
                std::process::Command::new(std::env::current_exe().unwrap())
                    .quiet()
                    .args(["--exact", "seat_process::tests::orphan_tool_helper"])
                    .env_remove("MAGI_PROCESS_OWNER")
                    .exec()
            );
        }
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let close_fds = std::env::var_os("MAGI_TEST_CLOSE_FDS").is_some();
        // Collect before fork, so the child only needs signal-safe close calls.
        let fds: Vec<_> = if close_fds {
            // SAFETY: sysconf/fcntl query process-local descriptor metadata.
            (3..unsafe { libc::sysconf(libc::_SC_OPEN_MAX) } as i32)
                .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
                .collect()
        } else {
            Vec::new()
        };
        // SAFETY: after fork the child only invokes async-signal-safe libc
        // functions, then exits by signal. It never reenters the Rust runtime.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                if libc::setsid() == -1 {
                    libc::_exit(2);
                }
                // Bound even a failing test's orphan lifetime.
                libc::alarm(10);
                libc::write(writer.as_raw_fd(), b"x".as_ptr().cast(), 1);
                for fd in &fds {
                    libc::close(*fd);
                }
                loop {
                    libc::pause();
                }
            }
        }
        use std::io::Read;
        let mut reader = reader;
        reader.read_exact(&mut [0]).unwrap();
        std::fs::write(path, pid.to_string()).unwrap();
        // Exit before Running's finish, orphaning the tool in its own group.
        std::process::exit(0);
    }
}
