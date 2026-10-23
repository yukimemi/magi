//! Own Chrome's descendants as well as the directly spawned browser. A panic
//! must not leave renderers or inherited test-runner handles alive (FAIL+LEAK).
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};

pub struct Chrome {
    child: Child,
    pid: u32,
    #[cfg(unix)]
    group: u32,
    #[cfg(windows)]
    job: Option<windows::Job>,
}

impl Chrome {
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        cmd.process_group(0);
        // Suspend before assignment: assigning a running Chrome races its first
        // renderer/utility spawn, which can then escape the job permanently.
        #[cfg(windows)]
        cmd.creation_flags(0x0000_0004); // CREATE_SUSPENDED
        let child = cmd.spawn()?;
        let mut chrome = Self {
            pid: child.id().expect("new child has a pid"),
            #[cfg(unix)]
            group: child.id().expect("new child has a pid"),
            child,
            #[cfg(windows)]
            job: None,
        };
        #[cfg(windows)]
        {
            chrome.job = Some(windows::Job::assign(&chrome.child)?);
            windows::resume(chrome.child.id().expect("new child has a pid"))?;
        }
        // Keep this mutable on all platforms for the Windows initialization.
        let _ = &mut chrome;
        Ok(chrome)
    }

    pub fn id(&self) -> Option<u32> {
        Some(self.pid)
    }
    pub fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.group as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
        let _ = self.child.start_kill();
        // Drop cannot await. Reap before profile deletion and before the runtime
        // disappears; kill_on_drop alone only schedules a kill of the root.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let root_done = !matches!(self.child.try_wait(), Ok(None));
            #[cfg(windows)]
            let tree_done = self.job.as_ref().is_none_or(windows::Job::empty);
            #[cfg(not(windows))]
            let tree_done = true;
            if root_done && tree_done {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("Chrome cleanup timed out: root_done={root_done}, tree_done={tree_done}");
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use tokio::process::Child;
    type Handle = *mut c_void;
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
    #[repr(C)]
    #[derive(Default)]
    struct Accounting {
        user_time: i64,
        kernel_time: i64,
        period_user: i64,
        period_kernel: i64,
        page_faults: u32,
        total: u32,
        active: u32,
        terminated: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct ThreadEntry {
        size: u32,
        usage: u32,
        id: u32,
        owner: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: Handle, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *const c_void, size: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, code: u32) -> i32;
        fn QueryInformationJobObject(
            job: Handle,
            class: i32,
            info: *mut c_void,
            size: u32,
            returned: *mut u32,
        ) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
        fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
        fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
        fn OpenThread(access: u32, inherit: i32, id: u32) -> Handle;
        fn ResumeThread(thread: Handle) -> u32;
    }
    pub struct Job(OwnedHandle);
    impl Job {
        pub fn assign(child: &Child) -> std::io::Result<Self> {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error());
                }
                let job = Self(OwnedHandle::from_raw_handle(handle));
                let mut limits = ExtendedLimits::default();
                limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                if SetInformationJobObject(
                    handle,
                    9,
                    std::ptr::from_ref(&limits).cast(),
                    std::mem::size_of::<ExtendedLimits>() as u32,
                ) == 0
                    || AssignProcessToJobObject(handle, child.raw_handle().expect("live child"))
                        == 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(job)
            }
        }
        pub fn terminate(&self) {
            if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
                eprintln!("terminate Chrome job: {}", std::io::Error::last_os_error());
            }
        }
        pub fn empty(&self) -> bool {
            let mut info = Accounting::default();
            unsafe {
                QueryInformationJobObject(
                    self.0.as_raw_handle(),
                    1,
                    std::ptr::from_mut(&mut info).cast(),
                    std::mem::size_of::<Accounting>() as u32,
                    std::ptr::null_mut(),
                ) != 0
                    && info.active == 0
            }
        }
    }

    /// CREATE_SUSPENDED leaves exactly one thread. Resume it only after job
    /// assignment; stable std/tokio do not expose the primary thread handle.
    pub fn resume(pid: u32) -> std::io::Result<()> {
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(4, 0); // TH32CS_SNAPTHREAD
            if snapshot as isize == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let snapshot = OwnedHandle::from_raw_handle(snapshot);
            let mut entry = ThreadEntry {
                size: std::mem::size_of::<ThreadEntry>() as u32,
                ..Default::default()
            };
            let mut found = Thread32First(snapshot.as_raw_handle(), &mut entry);
            while found != 0 {
                if entry.owner == pid {
                    let thread = OpenThread(2, 0, entry.id); // THREAD_SUSPEND_RESUME
                    if thread.is_null() {
                        return Err(std::io::Error::last_os_error());
                    }
                    let thread = OwnedHandle::from_raw_handle(thread);
                    if ResumeThread(thread.as_raw_handle()) == u32::MAX {
                        return Err(std::io::Error::last_os_error());
                    }
                    return Ok(());
                }
                found = Thread32Next(snapshot.as_raw_handle(), &mut entry);
            }
            Err(std::io::Error::other(
                "suspended Chrome primary thread not found",
            ))
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    const HELPER: &str = "cdp::process::tests::tree_helper";

    pub fn helper_command(marker: &std::path::Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", HELPER, "--nocapture"])
            .env("MAGI_CHROME_TREE_TEST", marker)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    }

    #[test]
    fn tree_helper() {
        let Some(marker) = std::env::var_os("MAGI_CHROME_TREE_TEST") else {
            return;
        };
        if std::env::var_os("MAGI_CHROME_TREE_LEAF").is_some() {
            let marker = std::path::PathBuf::from(marker);
            let temp = marker.with_extension("tmp");
            std::fs::write(&temp, std::process::id().to_string()).unwrap();
            std::fs::rename(temp, marker).unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", HELPER, "--nocapture"])
            .env("MAGI_CHROME_TREE_LEAF", "1")
            .spawn()
            .unwrap();
        // Intentionally wait forever with a live descendant and inherited
        // output handles. Only the outer process-tree owner can clean this up.
        let _ = child.wait();
    }

    #[tokio::test]
    async fn drop_reaps_root_and_terminates_descendant_on_unwind() {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("leaf-pid");
        let mut chrome = Chrome::spawn(&mut helper_command(&marker)).unwrap();
        let root = chrome.id().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !marker.exists() {
            assert!(chrome.try_wait().unwrap().is_none(), "helper exited early");
            assert!(Instant::now() < deadline, "descendant did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let leaf: u32 = std::fs::read_to_string(marker).unwrap().parse().unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _owner = chrome;
                panic!("exercise assertion cleanup");
            }))
            .is_err()
        );
        let mut system = System::new();
        loop {
            let pids = [Pid::from_u32(root), Pid::from_u32(leaf)];
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&pids),
                true,
                ProcessRefreshKind::nothing(),
            );
            // Unix can retain an orphan zombie until init reaps it; it has no
            // handles or execution left. Windows removes terminated processes.
            let alive = pids.iter().any(|pid| {
                system
                    .process(*pid)
                    .is_some_and(|p| p.status() != sysinfo::ProcessStatus::Zombie)
            });
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Chrome descendant survived cleanup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
