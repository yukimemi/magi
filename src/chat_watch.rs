//! Durable, supervised jobs started by a standing chat. Results use the same
//! draft/turn delivery as task completion notices; jobs never run twice.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::proc::Quiet as _;
use crate::talk::{self, Talks};

/// Default and maximum job durations, in seconds.
pub const DEFAULT_TIMEOUT: u64 = 900;
/// Long jobs should be filed as tasks instead.
pub const MAX_TIMEOUT: u64 = 3600;
/// Reservations count against this global limit too.
pub const MAX_CONCURRENT: usize = 4;
/// Maximum retained output, shared by stdout and stderr.
pub const OUTPUT_CAP: usize = 8192;

#[derive(Debug, Serialize, Deserialize)]
struct Job {
    id: String,
    talk: String,
    cwd: PathBuf,
    command: Vec<String>,
    timeout: u64,
    supervisor: Option<(u32, String)>,
    child: Option<(u32, String)>,
    result: Option<String>,
    tail: String,
    delivered: bool,
}

fn root(home: &Path) -> PathBuf {
    home.join("chat-watches")
}

struct Lock(File);

impl Drop for Lock {
    fn drop(&mut self) {
        unlock_file(&self.0);
    }
}

fn open_lock(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

fn lock(path: &Path) -> Result<Lock> {
    let file = open_lock(path)?;
    lock_file(&file, false)?;
    Ok(Lock(file))
}

fn try_lock(path: &Path) -> Result<Option<Lock>> {
    let file = open_lock(path)?;
    if lock_file(&file, true)? {
        Ok(Some(Lock(file)))
    } else {
        Ok(None)
    }
}

#[cfg(unix)]
fn lock_file(file: &File, immediate: bool) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    loop {
        if unsafe {
            libc::flock(
                file.as_raw_fd(),
                libc::LOCK_EX | if immediate { libc::LOCK_NB } else { 0 },
            )
        } == 0
        {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock if immediate => return Ok(false),
            _ => return Err(error),
        }
    }
}

#[cfg(unix)]
fn unlock_file(file: &File) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(windows)]
fn lock_file(file: &File, immediate: bool) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    windows::lock_file(file.as_raw_handle(), immediate)
}

#[cfg(windows)]
fn unlock_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    windows::unlock_file(file.as_raw_handle());
}

fn load(dir: &Path) -> Result<Job> {
    Ok(serde_json::from_slice(&std::fs::read(
        dir.join("job.json"),
    )?)?)
}

fn save(dir: &Path, job: &Job) -> Result<()> {
    let tmp = dir.join("job.tmp");
    std::fs::write(&tmp, serde_json::to_vec(job)?)?;
    std::fs::rename(tmp, dir.join("job.json"))?;
    Ok(())
}

fn directories(home: &Path) -> Result<Vec<PathBuf>> {
    Ok(std::fs::read_dir(root(home))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect())
}

/// Only the standing chat's own environment chooses the destination.
pub fn authorize(talks: &Talks, run: &str, node: &str) -> Result<String> {
    if node != crate::queue::CHAT_NODE || run.trim().is_empty() {
        bail!("talk watch requires MAGI_NODE=chat and MAGI_RUN from a standing chat");
    }
    let t = talks.get(run)?;
    if t.id != run || !t.status.open() {
        bail!("talk watch requires the exact id of an open conversation");
    }
    Ok(t.id)
}

/// Reserve a slot and detach a supervisor. Return only after it has recorded
/// startup (including an executable spawn failure, which is reported normally).
pub fn start(
    home: &Path,
    talks: &Talks,
    run: &str,
    node: &str,
    command: Vec<String>,
    timeout: u64,
) -> Result<String> {
    let talk = authorize(talks, run, node)?;
    if command.is_empty() || !(1..=MAX_TIMEOUT).contains(&timeout) {
        bail!("watch needs a command and a timeout between 1 and {MAX_TIMEOUT} seconds");
    }
    std::fs::create_dir_all(root(home))?;
    let _store = lock(&root(home).join("store.lock"))?;
    let active = directories(home)?
        .iter()
        .try_fold(0, |n, dir| -> Result<usize> {
            Ok(n + usize::from(load(dir)?.result.is_none()))
        })?;
    if active >= MAX_CONCURRENT {
        bail!("at most {MAX_CONCURRENT} chat watches may run concurrently");
    }
    let id = crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4();
    let dir = root(home).join(&id);
    let staging = root(home).join(format!(".new-{id}"));
    std::fs::create_dir(&staging)?;
    let mut job = Job {
        id: id.clone(),
        talk,
        cwd: std::env::current_dir()?,
        command,
        timeout,
        supervisor: None,
        child: None,
        result: None,
        tail: String::new(),
        delivered: false,
    };
    save(&staging, &job)?;
    std::fs::rename(&staging, &dir)?;
    let mut cmd = Command::new(crate::updater::current_exe()?);
    cmd.arg("talk")
        .arg("watch-run")
        .arg(home)
        .arg(&id)
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A separate session also survives the calling agent's group cleanup.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console to inherit,
        // and the calling agent's terminal must not end supervision.
        cmd.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let mut supervisor = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            job.result = Some(format!("supervisor startup failed: {e}"));
            save(&dir, &job)?;
            return Err(e.into());
        }
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let fresh = load(&dir)?;
        if fresh.supervisor.is_some() && (fresh.child.is_some() || fresh.result.is_some()) {
            // Reap while the calling process is alive; after it exits the
            // detached supervisor is reparented and reaped by the platform.
            std::thread::spawn(move || {
                let _ = supervisor.wait();
            });
            return Ok(id);
        }
        if supervisor.try_wait()?.is_some() || Instant::now() >= deadline {
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            bail!(
                "watch supervisor did not acknowledge startup; its durable reservation will be reconciled"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn identity(pid: u32) -> Result<(u32, String)> {
    Ok((
        pid,
        crate::proc::process_started_at(pid).context("cannot identify watch process")?,
    ))
}

fn append_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.extend_from_slice(bytes);
    if tail.len() > OUTPUT_CAP {
        tail.drain(..tail.len() - OUTPUT_CAP);
    }
}

fn reader(
    mut pipe: impl Pipe + Send + 'static,
    tail: Arc<Mutex<Vec<u8>>>,
    stopped: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0; 4096];
        if pipe.nonblocking().is_err() {
            return;
        }
        let mut final_budget = None;
        loop {
            // Drain buffered output after exit, but even an escaped process
            // continuously writing to an inherited pipe cannot keep us here.
            if stopped.load(Ordering::Acquire) {
                let remaining = final_budget.get_or_insert(OUTPUT_CAP * 16);
                if *remaining == 0 {
                    break;
                }
            }
            match pipe.read_available(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    append_tail(
                        &mut tail.lock().unwrap_or_else(|e| e.into_inner()),
                        &chunk[..n],
                    );
                    if let Some(remaining) = final_budget.as_mut() {
                        *remaining = remaining.saturating_sub(n);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if stopped.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    })
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // Every watch child gets its own group, so this also closes pipes
        // inherited by its descendants after the direct child exits.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: u32) {}

/// Internal supervisor entry point. The per-job OS lock is a durable lease:
/// released on process death, without age-based guesses or restarting the job.
pub fn supervise(home: &Path, id: &str) -> Result<()> {
    if id.contains(['/', '\\']) || id == "." || id == ".." {
        bail!("invalid watch id");
    }
    let dir = root(home).join(id);
    let _lease = lock(&dir.join("lease.lock"))?;
    let mut job = load(&dir)?;
    if job.result.is_some() || job.supervisor.is_some() {
        return Ok(());
    }
    job.supervisor = Some(identity(std::process::id())?);
    save(&dir, &job)?;
    let mut cmd = Command::new(crate::updater::current_exe()?);
    cmd.arg("talk")
        .arg("watch-exec")
        .arg(home)
        .arg(id)
        .current_dir(&job.cwd)
        .env_remove("MAGI_RUN")
        .env_remove("MAGI_NODE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.quiet();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            job.result = Some(format!("startup failed: {e}"));
            save(&dir, &job)?;
            return Ok(());
        }
    };
    #[cfg(windows)]
    let containment = match windows::Containment::assign(&child) {
        Ok(c) => c,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e.into());
        }
    };
    // If identity or persistence fails, do not leave an untracked process.
    let registered = identity(child.id()).and_then(|pid| {
        job.child = Some(pid);
        save(&dir, &job)
    });
    if let Err(e) = registered {
        kill_group(child.id());
        #[cfg(windows)]
        drop(containment);
        let _ = child.wait();
        return Err(e);
    }
    // The wrapper may execute only after its PID has been persisted. A
    // supervisor crash in the spawn/save gap leaves an inert wrapper, not an
    // untracked command. It exits when it detects the supervisor is gone.
    std::fs::write(dir.join("armed"), b"ready")?;
    let tail = Arc::new(Mutex::new(Vec::new()));
    let stopped = Arc::new(AtomicBool::new(false));
    let out = reader(
        child.stdout.take().context("missing stdout")?,
        tail.clone(),
        stopped.clone(),
    );
    let err = reader(
        child.stderr.take().context("missing stderr")?,
        tail.clone(),
        stopped.clone(),
    );
    let deadline = Instant::now() + Duration::from_secs(job.timeout);
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    break status.code().map_or_else(
                        || format!("terminated by signal {}", status.signal().unwrap_or(0)),
                        |code| format!("exited with status {code}"),
                    );
                }
                #[cfg(not(unix))]
                break status.code().map_or_else(
                    || "terminated without an exit code".to_owned(),
                    |code| format!("exited with status {code}"),
                );
            }
            Err(e) => break format!("result unknown: {e}"),
            Ok(None) if Instant::now() >= deadline => {
                break format!(
                    "timed out after {} second{}",
                    job.timeout,
                    if job.timeout == 1 { "" } else { "s" }
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    kill_group(child.id());
    #[cfg(windows)]
    drop(containment);
    let _ = child.wait();
    stopped.store(true, Ordering::Release);
    let _ = out.join();
    let _ = err.join();
    job.tail =
        String::from_utf8_lossy(&tail.lock().unwrap_or_else(|e| e.into_inner())).into_owned();
    job.result = Some(match std::fs::read_to_string(dir.join("spawn-error")) {
        Ok(error) => format!("startup failed: {error}"),
        Err(_) => outcome,
    });
    save(&dir, &job)
}

/// Reconcile completed or abandoned watches and queue each notice once. Lock
/// order is watch store, job lease, talk store. Transient errors are retried.
pub fn sweep(home: &Path, talks: &Talks) {
    if !root(home).exists() {
        return;
    }
    let sweep = || -> Result<()> {
        let _store = lock(&root(home).join("store.lock"))?;
        for dir in directories(home)? {
            let Some(_lease) = try_lock(&dir.join("lease.lock"))? else {
                continue;
            };
            let result = reconcile(&dir, talks);
            if let Err(e) = result {
                tracing::warn!("watch completion not queued: {e:#}");
            }
        }
        Ok(())
    };
    if let Err(e) = sweep() {
        tracing::warn!("watch sweep: {e:#}");
    }
}

fn reconcile(dir: &Path, talks: &Talks) -> Result<()> {
    let mut job = load(dir)?;
    if job.delivered {
        return Ok(());
    }
    if job.result.is_none() {
        if let Some((pid, started)) = &job.child {
            match (
                crate::proc::pid_status(*pid),
                crate::proc::process_started_at(*pid),
            ) {
                (Some(false), _) => kill_group(*pid),
                (Some(true), Some(current)) if current == *started => kill_group(*pid),
                (Some(true), Some(_)) => {} // PID reused: never kill it.
                _ => return Ok(()),         // Uncertain identity retains the slot.
            }
        }
        job.result =
            Some("result unknown: supervisor was lost; job will not be restarted".to_owned());
        save(dir, &job)?;
    }
    // Listing omits unreadable records, so it cannot prove deletion. Only
    // a missing path settles delivery; read/parse/permission errors retry.
    let gone = || {
        matches!(
            std::fs::symlink_metadata(talks.path_of(&job.talk)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        )
    };
    let mut t = match talks.get(&job.talk) {
        Ok(t) if t.status.open() => t,
        Ok(_) => {
            job.delivered = true;
            return save(dir, &job);
        }
        Err(_) if gone() => {
            job.delivered = true;
            return save(dir, &job);
        }
        Err(e) => return Err(e),
    };
    let (text, display) = crate::prompt::chat_watch_report(
        &job.id,
        job.result.as_deref().unwrap_or("unknown"),
        &job.tail,
    );
    if let Err(e) = talk::queue_notice(
        &mut t,
        talks,
        &text,
        &format!("watch:{}:finished", job.id),
        &|_| false,
        None,
        display,
    ) {
        match talks.get(&job.talk) {
            Ok(t) if !t.status.open() => {}
            Err(_) if gone() => {}
            _ => return Err(e),
        }
    }
    job.delivered = true;
    save(dir, &job)
}

/// The launch gate prevents commands from starting before their identity is
/// durable. This internal entry point cannot execute a job from another PID.
pub fn execute(home: &Path, id: &str) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;
    if id.contains(['/', '\\']) || id == "." || id == ".." {
        bail!("invalid watch id");
    }
    let dir = root(home).join(id);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let job = load(&dir)?;
        let (pid, started) = job.supervisor.as_ref().context("missing supervisor")?;
        if job.result.is_some()
            || crate::proc::pid_status(*pid) == Some(false)
            || crate::proc::process_started_at(*pid).is_some_and(|s| s != *started)
        {
            bail!("watch supervisor was lost before launch");
        }
        if dir.join("armed").exists() {
            if job.child.as_ref() != Some(&identity(std::process::id())?) {
                bail!("watch launch belongs to a different process");
            }
            let mut command = Command::new(&job.command[0]);
            command
                .args(&job.command[1..])
                .current_dir(&job.cwd)
                .env_remove("MAGI_RUN")
                .env_remove("MAGI_NODE");
            command.quiet();
            #[cfg(unix)]
            let error = command.exec();
            #[cfg(windows)]
            let error = match command.status() {
                Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                Err(error) => error,
            };
            std::fs::write(dir.join("spawn-error"), error.to_string())?;
            return Err(error.into());
        }
        if Instant::now() >= deadline {
            bail!("watch launch was not armed");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

trait Pipe: Read {
    fn nonblocking(&self) -> std::io::Result<()>;
    fn read_available(&mut self, bytes: &mut [u8]) -> std::io::Result<usize>;
}

#[cfg(unix)]
impl<T: Read + std::os::fd::AsRawFd> Pipe for T {
    fn nonblocking(&self) -> std::io::Result<()> {
        let fd = self.as_raw_fd();
        // Do not let escaped descendants keep the supervisor waiting on pipes.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
    fn read_available(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.read(bytes)
    }
}

#[cfg(windows)]
impl<T: Read + std::os::windows::io::AsRawHandle> Pipe for T {
    fn nonblocking(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn read_available(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let available = windows::available(self.as_raw_handle())?;
        if available == 0 {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        let count = bytes.len().min(available as usize);
        self.read(&mut bytes[..count])
    }
}

// Native job objects close the whole Windows process tree when a supervisor
// exits, even involuntarily. The launch gate stays closed until assignment.
#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::os::windows::io::AsRawHandle;
    type Handle = *mut c_void;
    #[repr(C)]
    #[derive(Default)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: Handle,
    }
    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
        process_time: i64,
        job_time: i64,
        flags: u32,
        min_working_set: usize,
        max_working_set: usize,
        active_processes: u32,
        affinity: usize,
        priority: u32,
        scheduling: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: [u64; 6],
        process_memory: usize,
        job_memory: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn LockFileEx(
            file: Handle,
            flags: u32,
            reserved: u32,
            bytes: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn UnlockFileEx(
            file: Handle,
            reserved: u32,
            bytes: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        fn CreateJobObjectW(attributes: Handle, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, size: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
        fn PeekNamedPipe(
            pipe: Handle,
            buffer: Handle,
            size: u32,
            read: *mut u32,
            available: *mut u32,
            left: *mut u32,
        ) -> i32;
    }
    pub fn lock_file(file: Handle, immediate: bool) -> std::io::Result<bool> {
        let mut overlapped = Overlapped::default();
        if unsafe { LockFileEx(file, 2 | u32::from(immediate), 0, 1, 0, &mut overlapped) } != 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if immediate && error.raw_os_error() == Some(33) {
            return Ok(false);
        }
        Err(error)
    }
    pub fn unlock_file(file: Handle) {
        let mut overlapped = Overlapped::default();
        unsafe {
            UnlockFileEx(file, 0, 1, 0, &mut overlapped);
        }
    }
    pub struct Containment(Handle);
    impl Containment {
        pub fn assign(child: &std::process::Child) -> std::io::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = Self(handle);
                let mut limits = ExtendedLimits::default();
                limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                if SetInformationJobObject(
                    handle,
                    9,
                    std::ptr::from_ref(&limits).cast(),
                    std::mem::size_of::<ExtendedLimits>() as u32,
                ) == 0
                    || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(job)
            }
        }
    }
    impl Drop for Containment {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    pub fn available(pipe: Handle) -> std::io::Result<u32> {
        let mut available = 0;
        if unsafe {
            PeekNamedPipe(
                pipe,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EndlessOutput;
    impl Read for EndlessOutput {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            bytes.fill(b'x');
            Ok(bytes.len())
        }
    }
    impl Pipe for EndlessOutput {
        fn nonblocking(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn read_available(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.read(bytes)
        }
    }

    #[test]
    fn stopped_readers_finish_even_if_an_inherited_pipe_never_stops_writing() {
        let tail = Arc::new(Mutex::new(Vec::new()));
        reader(EndlessOutput, tail.clone(), Arc::new(AtomicBool::new(true)))
            .join()
            .expect("reader finished");
        assert_eq!(tail.lock().expect("tail").len(), OUTPUT_CAP);
    }
}
