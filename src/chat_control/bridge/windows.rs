//! Local named pipes survive CLI/shell tool descendants without handle inheritance.
use std::ffi::c_void;
use std::os::windows::{fs::OpenOptionsExt as _, io::IntoRawHandle as _};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};

use super::{MAX_FRAME, Operation, Request, Response};

pub const CHANNEL_ENV: &str = "MAGI_CHAT_CONTROL_PIPE";
// Avoid KEY/SECRET/TOKEN: shell environment policies may strip those names.
const CAP_ENV: &str = "MAGI_CHAT_CONTROL_CAP";
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptGenRandom(algorithm: *mut c_void, buffer: *mut u8, length: u32, flags: u32) -> i32;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        text: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
}

fn secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    // SAFETY: system-preferred RNG needs no algorithm handle; buffer is writable.
    let status = unsafe { BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), 32, 2) };
    ensure!(status >= 0, "cannot generate chat control capability");
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn listen(name: &str, first: bool) -> Result<NamedPipeServer> {
    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    // Only the owner may create additional instances. Local clients (including
    // restricted sandbox tokens) receive data I/O and synchronization rights on
    // this pipe alone; the secret authenticates them before any operation.
    // GENERIC_WRITE would also grant FILE_CREATE_PIPE_INSTANCE: do not use it.
    let sddl: Vec<u16> = "D:P(A;;GA;;;OW)(A;;0x00100003;;;WD)(A;;0x00100003;;;RC)\0"
        .encode_utf16()
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: terminated UTF-16 input and valid output pointer.
    ensure!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } != 0,
        "create chat control security descriptor: {}",
        std::io::Error::last_os_error()
    );
    let mut attrs = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit: 0,
    };
    // SAFETY: descriptor and attributes remain alive through CreateNamedPipeW;
    // Windows copies the descriptor and the handle is not inheritable.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attrs as *mut SecurityAttributes).cast(),
            )
    };
    // SAFETY: the conversion API allocated this descriptor with LocalAlloc.
    unsafe {
        LocalFree(descriptor);
    }
    Ok(result?)
}

fn connect(name: &str) -> std::io::Result<NamedPipeClient> {
    let file = std::fs::OpenOptions::new()
        // FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZE, without the right to
        // create a server instance under the same name.
        .access_mode(0x0010_0003)
        // OVERLAPPED, SQOS_PRESENT, IDENTIFICATION: never allow impersonation.
        .custom_flags(0x4000_0000 | 0x0010_0000 | 0x0001_0000)
        .open(name)?;
    // SAFETY: transfer a newly owned overlapped pipe handle to Tokio. Tokio
    // takes ownership of the handle even when registration fails.
    unsafe { NamedPipeClient::from_raw_handle(file.into_raw_handle()) }
}

pub(crate) struct Bridge {
    name: String,
    token: String,
    active: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

impl Bridge {
    pub fn start(home: &Path, talk: &str, _artifacts: &Path) -> Result<Self> {
        let talks = crate::talk::Talks::at(home.join("talks"));
        super::super::authorize(&talks, talk, crate::queue::CHAT_NODE)?;
        let name = format!(r"\\.\pipe\magi-chat-{}", secret()?);
        let token = secret()?;
        let mut incoming = listen(&name, true)?;
        let home = home.to_owned();
        let talk = talk.to_owned();
        let active = Arc::new(AtomicBool::new(true));
        let server = tokio::spawn({
            let name = name.clone();
            let token = token.clone();
            let active = active.clone();
            async move {
                // Dropping the listener task also aborts every connected handler.
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        result = incoming.connect(), if connections.len() < 32 => {
                            if result.is_err() { break; }
                            let next = match listen(&name, false) {
                                Ok(next) => next,
                                Err(_) => break,
                            };
                            let mut pipe = std::mem::replace(&mut incoming, next);
                            let home = home.clone();
                            let talk = talk.clone();
                            let token = token.clone();
                            let active = active.clone();
                            connections.spawn(async move {
                                let _ = tokio::time::timeout(IO_TIMEOUT, async {
                                    // Authenticate before parsing or executing an operation.
                                    let mut supplied = [0; 64];
                                    pipe.read_exact(&mut supplied).await?;
                                    ensure!(supplied.as_slice() == token.as_bytes(), "invalid control capability");
                                    let len = pipe.read_u32().await? as usize;
                                    ensure!(len <= MAX_FRAME, "control request too large");
                                    let mut body = vec![0; len];
                                    pipe.read_exact(&mut body).await?;
                                    let Request { id, operation } = serde_json::from_slice(&body)?;
                                    ensure!(active.load(Ordering::Acquire), "control capability revoked");
                                    let talks = crate::talk::Talks::at(home.join("talks"));
                                    let result = super::super::execute(
                                        &crate::queue::Queue::at(home.join("queue")),
                                        &home, &talks, &talk, operation,
                                    ).await.map_err(|e| format!("{e:#}"));
                                    let response = serde_json::to_vec(&Response { id, result })?;
                                    ensure!(response.len() <= MAX_FRAME, "control response too large");
                                    pipe.write_u32(response.len() as u32).await?;
                                    pipe.write_all(&response).await?;
                                    // Keep the server handle alive until the client reads the response.
                                    let mut done = [0];
                                    let _ = pipe.read(&mut done).await?;
                                    Ok::<_, anyhow::Error>(())
                                }).await;
                            });
                        }
                        _ = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            }
        });
        Ok(Self {
            name,
            token,
            active,
            server,
        })
    }

    pub fn attach(&self, command: &mut tokio::process::Command) {
        command
            .env(CHANNEL_ENV, &self.name)
            .env(CAP_ENV, &self.token);
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.server.abort();
    }
}

pub(crate) fn clear_environment(command: &mut tokio::process::Command) {
    for name in [
        CHANNEL_ENV,
        CAP_ENV,
        "MAGI_CHAT_CONTROL_FD",
        "MAGI_CHAT_CONTROL_RESPONSE_FD",
        "MAGI_CHAT_CONTROL_LOCK_FD",
    ] {
        command.env_remove(name);
    }
}

pub async fn request(operation: &Operation) -> Result<String> {
    let name = std::env::var(CHANNEL_ENV)?;
    ensure!(
        name.starts_with(r"\\.\pipe\magi-chat-"),
        "invalid control pipe"
    );
    let token = std::env::var(CAP_ENV).context("missing control capability")?;
    ensure!(token.len() == 64, "invalid control capability");
    request_at(&name, &token, operation).await
}

async fn request_at(name: &str, token: &str, operation: &Operation) -> Result<String> {
    tokio::time::timeout(IO_TIMEOUT, async {
        let mut pipe = loop {
            match connect(name) {
                Ok(pipe) => break pipe,
                // ERROR_PIPE_BUSY: another connection is being accepted.
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let id = crate::rng::entropy();
        let body = serde_json::to_vec(&serde_json::json!({ "id": id, "operation": operation }))?;
        ensure!(body.len() <= MAX_FRAME, "control request exceeds 1 MiB");
        pipe.write_all(token.as_bytes()).await?;
        pipe.write_u32(body.len() as u32).await?;
        pipe.write_all(&body).await?;
        let len = pipe.read_u32().await? as usize;
        ensure!(len <= MAX_FRAME, "control response too large");
        let mut body = vec![0; len];
        pipe.read_exact(&mut body).await?;
        let response: Response = serde_json::from_slice(&body)?;
        ensure!(response.id == id, "control response association lost");
        response.result.map_err(anyhow::Error::msg)
    })
    .await
    .context("chat control timed out; operation outcome is unknown; do not automatically retry")?
    .context("chat control failed; operation outcome may be unknown; do not automatically retry")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Queue, Source, Task, TaskStatus};

    #[tokio::test]
    async fn authenticated_connections_are_isolated_and_revoked() {
        let tmp = tempfile::tempdir().unwrap();
        let talks = crate::talk::Talks::at(tmp.path().join("talks"));
        let mut talk: crate::talk::Talk = serde_json::from_value(serde_json::json!({
            "schema": 1, "id": "talk-pipe", "repo": ".", "agent": "mock",
            "status": "open", "turns": [], "created_at": jiff::Timestamp::now(),
            "updated_at": jiff::Timestamp::now(),
            "seat": crate::agent::SeatState::new("talk", "mock", 1)
        }))
        .unwrap();
        talks.put(&mut talk).unwrap();
        let queue = Queue::at(tmp.path().join("queue"));
        let mut tasks = Vec::new();
        for owner in [talk.id.as_str(), talk.id.as_str(), "other-talk"] {
            let mut task = Task::new(
                "work".into(),
                "work".into(),
                tmp.path().into(),
                Source::Agent {
                    run: owner.to_owned(),
                    node: crate::queue::CHAT_NODE.into(),
                },
            );
            queue.put(&mut task).unwrap();
            tasks.push(task);
        }
        let bridge = Bridge::start(tmp.path(), &talk.id, tmp.path()).unwrap();
        let mut command = tokio::process::Command::new("unused");
        bridge.attach(&mut command);
        let environment: Vec<_> = command.as_std().get_envs().collect();
        // The capability must survive automatic credential-name exclusions.
        for (name, value) in &environment {
            let name = name.to_str().unwrap().to_ascii_uppercase();
            assert!(
                !["KEY", "SECRET", "TOKEN"]
                    .iter()
                    .any(|word| name.contains(*word))
            );
            assert!(value.is_some());
        }
        assert!(environment.iter().any(|(name, value)| {
            *name == CAP_ENV && *value == Some(std::ffi::OsStr::new(&bridge.token))
        }));
        clear_environment(&mut command);
        assert!(
            command
                .as_std()
                .get_envs()
                .all(|(_, value)| value.is_none())
        );
        let hold = |index: usize| Operation::Hold {
            task: tasks[index].id.clone(),
            reason: "wait".into(),
        };
        let first = hold(0);
        let second = hold(1);
        let (a, b) = tokio::join!(
            request_at(&bridge.name, &bridge.token, &first),
            request_at(&bridge.name, &bridge.token, &second),
        );
        assert!(a.is_ok(), "{a:?}");
        assert!(b.is_ok(), "{b:?}");
        assert_eq!(queue.get(&tasks[0].id).unwrap().status, TaskStatus::Held);
        assert_eq!(queue.get(&tasks[1].id).unwrap().status, TaskStatus::Held);
        // A task another conversation filed is controlled like any other: the
        // owner's chat request is not limited to the tasks this chat filed.
        let other = request_at(&bridge.name, &bridge.token, &hold(2)).await;
        assert!(other.is_ok(), "{other:?}");
        assert_eq!(queue.get(&tasks[2].id).unwrap().status, TaskStatus::Held);
        let release = Operation::Release {
            task: tasks[0].id.clone(),
        };
        assert!(
            request_at(&bridge.name, &"0".repeat(64), &release)
                .await
                .is_err()
        );
        assert_eq!(queue.get(&tasks[0].id).unwrap().status, TaskStatus::Held);
        // A connection accepted before revocation may not submit work afterwards.
        let mut pending = connect(&bridge.name).unwrap();
        pending
            .write_all(&bridge.token.as_bytes()[..32])
            .await
            .unwrap();
        tokio::task::yield_now().await;
        let name = bridge.name.clone();
        let token = bridge.token.clone();
        drop(bridge);
        tokio::task::yield_now().await;
        let _ = pending.write_all(&token.as_bytes()[32..]).await;
        let body =
            serde_json::to_vec(&serde_json::json!({ "id": 99, "operation": release })).unwrap();
        let _ = pending.write_u32(body.len() as u32).await;
        let _ = pending.write_all(&body).await;
        let mut byte = [0];
        let closed = tokio::time::timeout(Duration::from_secs(2), pending.read(&mut byte))
            .await
            .unwrap();
        assert!(closed.is_err() || closed.unwrap() == 0);
        assert!(request_at(&name, &token, &release).await.is_err());
        assert_eq!(queue.get(&tasks[0].id).unwrap().status, TaskStatus::Held);
    }

    #[tokio::test]
    async fn lost_or_mismatched_responses_are_uncertain_and_never_retried() {
        for mismatch in [false, true] {
            let name = format!(r"\\.\pipe\magi-chat-{}", secret().unwrap());
            let mut server = listen(&name, true).unwrap();
            let worker = tokio::spawn(async move {
                server.connect().await.unwrap();
                let mut token = [0; 64];
                server.read_exact(&mut token).await.unwrap();
                let len = server.read_u32().await.unwrap() as usize;
                let mut body = vec![0; len];
                server.read_exact(&mut body).await.unwrap();
                if mismatch {
                    let request: Request = serde_json::from_slice(&body).unwrap();
                    let response = serde_json::to_vec(&Response {
                        id: request.id.wrapping_add(1),
                        result: Ok("wrong".into()),
                    })
                    .unwrap();
                    server.write_u32(response.len() as u32).await.unwrap();
                    server.write_all(&response).await.unwrap();
                    let mut done = [0];
                    let _ = server.read(&mut done).await;
                }
            });
            let error = request_at(
                &name,
                &"0".repeat(64),
                &Operation::Release {
                    task: "task".into(),
                },
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("outcome"));
            worker.await.unwrap();
        }
    }
}
