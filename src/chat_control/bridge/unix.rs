//! A turn-scoped inherited channel, without filesystem or network grants.
//!
//! The launcher executes typed requests as the talk fixed at startup. The
//! channel cannot name a different talk, execute a command, or approve a merge.
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
const MAX_STALE_RESPONSES: usize = 64;

/// Only descriptor preparation failures are safe to fall back from: no request
/// bytes have been sent, so local execution cannot duplicate an operation.
#[derive(Debug, thiserror::Error)]
#[error("prepare chat control descriptors")]
pub(crate) struct Unavailable;

pub(crate) struct Bridge {
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
                    let result = super::super::execute(
                        &crate::queue::Queue::at(home.join("queue")),
                        &home,
                        &talks,
                        &talk,
                        operation,
                    )
                    .await
                    .map_err(|e| format!("{e:#}"));
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

pub(crate) fn clear_environment(command: &mut tokio::process::Command) {
    command
        .env_remove(CHANNEL_ENV)
        .env_remove(RESPONSE_ENV)
        .env_remove(LOCK_ENV)
        .env_remove("MAGI_CHAT_CONTROL_PIPE")
        .env_remove("MAGI_CHAT_CONTROL_CAP");
}

fn duplicate(name: &str) -> Result<RawFd> {
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
    Ok(copy)
}

pub fn request(operation: &Operation) -> Result<String> {
    let id = crate::rng::entropy();
    let body = serde_json::to_vec(&serde_json::json!({ "id": id, "operation": operation }))?;
    ensure!(body.len() <= MAX_FRAME, "control request exceeds 1 MiB");
    let (lock, mut requests, mut responses) = prepare().context(Unavailable)?;
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
    // SAFETY: duplicate() transfers ownership of fresh descriptors. Each file
    // is immediately owned so later preparation failures close earlier copies.
    let lock = unsafe { std::fs::File::from_raw_fd(duplicate(LOCK_ENV)?) };
    let requests = unsafe { std::fs::File::from_raw_fd(duplicate(CHANNEL_ENV)?) };
    let responses = unsafe { std::fs::File::from_raw_fd(duplicate(RESPONSE_ENV)?) };
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
