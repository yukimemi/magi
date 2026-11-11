//! Turn-scoped inherited pipes plus authenticated local socket recovery.
//!
//! The launcher executes typed [`crate::chat_control::Operation`]s as the talk fixed
//! at startup. The channel cannot name a different talk or execute a command.
//! An unlinked file descriptor supplies process-associated POSIX record locks
//! so concurrent CLI tool calls cannot interleave frames on the shared pipes.
use std::io::Write as _;
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{MAX_FRAME, Operation, Request, Response};

pub const CHANNEL_ENV: &str = "MAGI_CHAT_CONTROL_FD";
const RESPONSE_ENV: &str = "MAGI_CHAT_CONTROL_RESPONSE_FD";
const LOCK_ENV: &str = "MAGI_CHAT_CONTROL_LOCK_FD";
mod socket;
const IDENTITY_ENV: &str = "MAGI_CHAT_CONTROL_IDENTITIES";
const MAX_STALE_RESPONSES: usize = 64;

/// Only transport preparation failures are safe to fall back from: no request
/// bytes have been sent, so local execution cannot duplicate an operation.
#[derive(Debug, thiserror::Error)]
#[error("prepare authenticated chat control transport (no operation sent)")]
pub(crate) struct Unavailable;

pub(crate) struct Bridge {
    socket: Option<socket::Socket>,
    requests: std::fs::File,
    responses: std::fs::File,
    lock: std::fs::File,
    server: tokio::task::JoinHandle<()>,
}

impl Bridge {
    /// Failure disables controls for this invocation, never the conversation.
    pub fn start(home: &Path, talk: &str, artifacts: &Path) -> Result<Self> {
        let talks = crate::talk::Talks::at(home.join("talks"));
        super::super::authorize(&talks, talk, crate::queue::CHAT_NODE)?;
        // Anonymous pipes need neither a path grant nor network access.
        let (requests, mut incoming) = tokio::net::unix::pipe::pipe()?;
        let (mut outgoing, responses) = tokio::net::unix::pipe::pipe()?;
        let requests = std::fs::File::from(requests.into_blocking_fd()?);
        let responses = std::fs::File::from(responses.into_blocking_fd()?);
        let path = artifacts.join(format!(".control-{}.lock", crate::rng::entropy()));
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        std::fs::remove_file(path)?;
        let serial = socket::Serial::default();
        let socket = match socket::Socket::start(home, talk, serial.clone()) {
            Ok(socket) => Some(socket),
            Err(error) => {
                tracing::warn!(%error, "chat control socket unavailable; inherited descriptor transport remains available");
                None
            }
        };
        let home = home.to_owned();
        let talk = talk.to_owned();
        let server = tokio::spawn(async move {
            let result: Result<()> = async {
                loop {
                    let len = incoming.read_u32().await? as usize;
                    ensure!(len <= MAX_FRAME, "control request too large");
                    let mut body = vec![0; len];
                    incoming.read_exact(&mut body).await?;
                    let Request { id, operation } = serde_json::from_slice(&body)?;
                    let _guard = serial.lock().await;
                    let result = super::super::execute(
                        &crate::queue::Queue::at(home.join("queue")),
                        &home,
                        &talks,
                        &talk,
                        operation,
                    )
                    .await
                    .map_err(|e| format!("{e:#}"));
                    drop(_guard);
                    let response = serde_json::to_vec(&Response { id, result })?;
                    ensure!(response.len() <= MAX_FRAME, "control response too large");
                    outgoing.write_u32(response.len() as u32).await?;
                    outgoing.write_all(&response).await?;
                }
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, "chat control channel closed");
            }
        });
        Ok(Self {
            socket,
            requests,
            responses,
            lock,
            server,
        })
    }

    pub fn attach(&self, command: &mut tokio::process::Command) {
        let requests = self.requests.as_raw_fd();
        let responses = self.responses.as_raw_fd();
        let lock = self.lock.as_raw_fd();
        if let Some(socket) = &self.socket {
            socket.attach(command);
        }
        let identities: Vec<_> = [requests, responses, lock]
            .into_iter()
            .map(identity)
            .collect::<Result<_>>()
            .expect("launcher owns control descriptors");
        command.env(
            IDENTITY_ENV,
            serde_json::to_string(&identities).expect("descriptor identities serialize"),
        );
        command
            .env(CHANNEL_ENV, requests.to_string())
            .env(RESPONSE_ENV, responses.to_string())
            .env(LOCK_ENV, lock.to_string());
        // SAFETY: fcntl is async-signal-safe. Clear CLOEXEC only after fork,
        // leaving the launcher's descriptors private to unrelated children.
        unsafe {
            command.pre_exec(move || {
                for fd in [requests, responses, lock] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Cancellation, fallbacks and normal completion revoke the capability.
        self.server.abort();
    }
}

/// Drop inherited authority in detached children, as well as removing its env.
/// Inspect in the parent; the child hook uses only async-signal-safe syscalls.
pub(super) fn close_inherited(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt as _;
    let Ok(encoded) = std::env::var(IDENTITY_ENV) else {
        return;
    };
    let Ok(identities) = serde_json::from_str::<Vec<[String; 4]>>(&encoded) else {
        return;
    };
    let mut descriptors = Vec::new();
    for (name, expected) in [CHANNEL_ENV, RESPONSE_ENV, LOCK_ENV]
        .into_iter()
        .zip(identities)
    {
        let Some(fd) = std::env::var(name)
            .ok()
            .and_then(|s| s.parse::<RawFd>().ok())
            .filter(|fd| *fd > 2)
        else {
            continue;
        };
        if identity(fd).ok().as_ref() != Some(&expected) {
            continue;
        }
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } == 0 {
            descriptors.push((fd, stat.st_dev, stat.st_ino, stat.st_mode));
        }
    }
    unsafe {
        command.pre_exec(move || {
            for &(fd, dev, ino, mode) in &descriptors {
                let mut stat: libc::stat = std::mem::zeroed();
                if libc::fstat(fd, &mut stat) == 0
                    && stat.st_dev == dev
                    && stat.st_ino == ino
                    && stat.st_mode == mode
                {
                    libc::close(fd);
                }
            }
            Ok(())
        });
    }
}

fn duplicate(name: &str, expected: &[String; 4]) -> Result<RawFd> {
    let fd: RawFd = std::env::var(name)?.parse()?;
    ensure!(fd > 2, "invalid control descriptor");
    // SAFETY: duplicate an existing inherited descriptor; the caller takes
    // ownership only of the new descriptor. Never close the original.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    ensure!(
        copy != -1,
        "control channel is unavailable: {}",
        std::io::Error::last_os_error()
    );
    // Own the duplicate immediately, including on identity failure. Never
    // read/write a reused fd merely because its number was inherited in env.
    let file = unsafe { std::fs::File::from_raw_fd(copy) };
    ensure!(
        &identity(file.as_raw_fd())? == expected,
        "control descriptor identity mismatch: {name}"
    );
    use std::os::fd::IntoRawFd as _;
    Ok(file.into_raw_fd())
}

fn identity(fd: RawFd) -> Result<[String; 4]> {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::fstat(fd, &mut stat) } == 0,
        "control descriptor closed: {}",
        std::io::Error::last_os_error()
    );
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    ensure!(flags != -1, "cannot inspect control descriptor");
    Ok([
        stat.st_dev.to_string(),
        stat.st_ino.to_string(),
        (stat.st_mode & libc::S_IFMT).to_string(),
        (flags & libc::O_ACCMODE).to_string(),
    ])
}

pub fn request(operation: &Operation) -> Result<String> {
    let id = crate::rng::entropy();
    let body = serde_json::to_vec(&serde_json::json!({ "id": id, "operation": operation }))?;
    ensure!(body.len() <= MAX_FRAME, "control request exceeds 1 MiB");
    let (lock, mut requests, mut responses) = match prepare() {
        Ok(descriptors) => descriptors,
        Err(error) => {
            return socket::request(&body, id)
                .with_context(|| format!("inherited control descriptors unavailable: {error:#}"));
        }
    };
    // POSIX locks belong to each process (unlike flock's inherited open-file
    // description lock). Closing `lock` releases this command's reservation.
    let mut record: libc::flock = unsafe { std::mem::zeroed() };
    record.l_type = libc::F_WRLCK as _;
    record.l_whence = libc::SEEK_SET as _;
    loop {
        // SAFETY: record is initialized and lives through the blocking call.
        if unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETLKW, &record) } != -1 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            bail!("lock control channel: {error}");
        }
    }
    // After transmission starts, never retry or execute locally: the launcher
    // may already have applied the operation even if the response is lost.
    (|| -> Result<Response> {
        requests.write_all(&(body.len() as u32).to_be_bytes())?;
        requests.write_all(&body)?;
        receive(&mut responses, id)
    })()
    .context(
        "chat control operation may have executed; outcome is unknown; do not automatically retry",
    )?
    .result
    .map_err(anyhow::Error::msg)
}

fn prepare() -> Result<(std::fs::File, std::fs::File, std::fs::File)> {
    let identities: Vec<[String; 4]> = serde_json::from_str(
        &std::env::var(IDENTITY_ENV).context("missing launcher descriptor identities")?,
    )?;
    ensure!(
        identities.len() == 3,
        "invalid launcher descriptor identities"
    );
    ensure!(
        identities[0][2] == libc::S_IFIFO.to_string()
            && identities[1][2] == libc::S_IFIFO.to_string()
            && identities[2][2] == libc::S_IFREG.to_string(),
        "invalid control descriptor types"
    );
    // SAFETY: duplicate() transfers ownership of fresh descriptors. Each file
    // is immediately owned so later preparation failures close earlier copies.
    let lock = unsafe { std::fs::File::from_raw_fd(duplicate(LOCK_ENV, &identities[2])?) };
    let requests = unsafe { std::fs::File::from_raw_fd(duplicate(CHANNEL_ENV, &identities[0])?) };
    let responses = unsafe { std::fs::File::from_raw_fd(duplicate(RESPONSE_ENV, &identities[1])?) };
    Ok((lock, requests, responses))
}

fn receive(responses: &mut impl std::io::Read, id: u64) -> Result<Response> {
    // A killed requester releases its process lock but can leave its complete
    // response in the pipe. Drain those frames under the same lock, including
    // error responses, without sending the operation again. Bound the number
    // drained, not the duration of a legitimate long-running operation.
    for stale in 0..=MAX_STALE_RESPONSES {
        let mut length = [0; 4];
        responses
            .read_exact(&mut length)
            .context("chat control launcher disconnected")?;
        let length = u32::from_be_bytes(length) as usize;
        ensure!(length <= MAX_FRAME, "control response too large");
        let mut response = vec![0; length];
        responses.read_exact(&mut response)?;
        let response: Response = serde_json::from_slice(&response)?;
        if response.id == id {
            return Ok(response);
        }
        ensure!(
            stale < MAX_STALE_RESPONSES,
            "too many stale control responses"
        );
    }
    unreachable!("response drain is bounded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "subprocess helper for detached control cleanup"]
    fn detached_control_cleanup_child() {
        if std::env::var_os("MAGI_TEST_CONTROL_CLEANUP").is_none() {
            return;
        }
        let descriptors: Vec<_> = [CHANNEL_ENV, RESPONSE_ENV, LOCK_ENV]
            .into_iter()
            .map(|name| std::env::var(name).unwrap())
            .collect();
        let mut command = std::process::Command::new("python3");
        command
            .args([
                "-c",
                r#"import os, sys
assert not any(n.startswith('MAGI_CHAT_CONTROL_') for n in os.environ)
for fd in map(int, sys.argv[1:]):
    try:
        os.fstat(fd)
    except OSError:
        pass
    else:
        raise AssertionError('detached child retained control descriptor')
"#,
            ])
            .args(descriptors);
        super::super::clear_std_environment(&mut command);
        assert!(command.status().unwrap().success());
    }

    #[tokio::test]
    async fn detached_children_lose_control_environment_and_descriptors() {
        let tmp = tempfile::tempdir().unwrap();
        let talks = crate::talk::Talks::at(tmp.path().join("talks"));
        let mut talk: crate::talk::Talk = serde_json::from_value(serde_json::json!({
            "schema": 1, "id": "cleanup-chat", "repo": ".", "agent": "mock",
            "status": "open", "turns": [], "created_at": jiff::Timestamp::now(),
            "updated_at": jiff::Timestamp::now(),
            "seat": crate::agent::SeatState::new("talk", "mock", 1)
        }))
        .unwrap();
        talks.put(&mut talk).unwrap();
        let bridge = Bridge::start(tmp.path(), &talk.id, tmp.path()).unwrap();
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--ignored",
                "--exact",
                "chat_control::bridge::unix::tests::detached_control_cleanup_child",
            ])
            .env("MAGI_TEST_CONTROL_CLEANUP", "1");
        bridge.attach(&mut command);
        let output = command.output().await.unwrap();
        assert!(
            output.status.success(),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn frame(id: u64, result: std::result::Result<String, String>) -> Vec<u8> {
        let body = serde_json::to_vec(&Response { id, result }).unwrap();
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend(body);
        bytes
    }

    #[test]
    fn drains_stale_successes_and_errors_up_to_the_limit() {
        let mut bytes = Vec::new();
        for id in 0..MAX_STALE_RESPONSES as u64 {
            bytes.extend(frame(
                id,
                if id % 2 == 0 {
                    Ok("old".into())
                } else {
                    Err("old error".into())
                },
            ));
        }
        bytes.extend(frame(100, Ok("mine".into())));
        assert_eq!(
            receive(&mut bytes.as_slice(), 100).unwrap().result.unwrap(),
            "mine"
        );
        assert_eq!(
            receive(&mut frame(100, Err("mine failed".into())).as_slice(), 100)
                .unwrap()
                .result
                .unwrap_err(),
            "mine failed"
        );
    }

    #[test]
    fn rejects_excess_stale_responses_and_invalid_frames() {
        let mut bytes = Vec::new();
        for _ in 0..=MAX_STALE_RESPONSES {
            bytes.extend(frame(1, Ok("old".into())));
        }
        bytes.extend(frame(2, Ok("mine".into())));
        let mut remaining = bytes.as_slice();
        assert!(
            receive(&mut remaining, 2)
                .err()
                .unwrap()
                .to_string()
                .contains("too many stale")
        );
        assert_eq!(receive(&mut remaining, 2).unwrap().result.unwrap(), "mine");
        for bytes in [
            vec![],
            vec![0, 0],
            vec![0, 0, 0, 2, b'{'],
            vec![0, 0, 0, 1, b'!'],
            ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec(),
        ] {
            assert!(receive(&mut bytes.as_slice(), 2).is_err());
        }
    }
}
