//! Capability-authenticated transport for backends that close inherited fds.
use super::*;
use std::io::Read as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub(super) const SOCKET_ENV: &str = "MAGI_CHAT_CONTROL_SOCKET";
const CAP_ENV: &str = "MAGI_CHAT_CONTROL_CAP";
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) type Serial = Arc<tokio::sync::Mutex<()>>;

pub(super) struct Socket {
    directory: std::path::PathBuf,
    token: String,
    active: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
}

fn secret() -> Result<String> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn socket_directory(
    bases: impl IntoIterator<Item = std::path::PathBuf>,
) -> Result<std::path::PathBuf> {
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut failure = anyhow::anyhow!("no temporary directory candidates");
    for base in bases {
        let directory = base.join(format!("magi-control-{}", &secret()?[..24]));
        // 終端 NUL を含めて、ホストの sun_path に収まる必要がある。
        if directory.join("channel").as_os_str().as_bytes().len() >= address.sun_path.len() {
            failure = anyhow::anyhow!("chat control socket path exceeds sun_path capacity");
            continue;
        }
        match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => return Ok(directory),
            Err(error) => failure = error.into(),
        }
    }
    Err(failure).context("cannot create a private directory for the chat control socket")
}

impl Socket {
    pub(super) fn start(home: &Path, talk: &str, serial: Serial) -> Result<Self> {
        let token = secret()?;
        let directory = socket_directory([std::env::temp_dir(), "/tmp".into()])?;
        let listener = match (|| -> Result<_> {
            let listener = tokio::net::UnixListener::bind(directory.join("channel"))?;
            std::fs::set_permissions(
                directory.join("channel"),
                std::fs::Permissions::from_mode(0o600),
            )?;
            Ok(listener)
        })() {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&directory);
                return Err(error);
            }
        };
        let active = Arc::new(AtomicBool::new(true));
        let server = tokio::spawn({
            let token = token.clone();
            let active = active.clone();
            let home = home.to_owned();
            let talk = talk.to_owned();
            async move {
                // Aborting this task drops and aborts all in-flight handlers.
                let mut clients = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept(), if clients.len() < 32 => {
                            let Ok((mut stream, _)) = accepted else { break };
                            let token = token.clone();
                            let home = home.clone();
                            let talk = talk.clone();
                            let active = active.clone();
                            let serial = serial.clone();
                            clients.spawn(async move {
                                let _ = async {
                                    let request = tokio::time::timeout(AUTH_TIMEOUT, async {
                                        ensure!(stream.peer_cred()?.uid() == unsafe { libc::geteuid() }, "control peer UID mismatch");
                                        let mut supplied = [0; 64];
                                        stream.read_exact(&mut supplied).await?;
                                        ensure!(super::super::constant_eq(&supplied, token.as_bytes()), "control authentication rejected");
                                        // No operation has been sent before this ACK.
                                        stream.write_all(b"A").await?;
                                        let len = stream.read_u32().await? as usize;
                                        ensure!(len <= MAX_FRAME, "control request too large");
                                        let mut body = vec![0; len];
                                        stream.read_exact(&mut body).await?;
                                        Ok::<Request, anyhow::Error>(serde_json::from_slice(&body)?)
                                    }).await??;
                                    let _guard = serial.lock().await;
                                    ensure!(active.load(Ordering::Acquire), "control capability revoked");
                                    let talks = crate::talk::Talks::at(home.join("talks"));
                                    let result = super::super::super::execute(
                                        &crate::queue::Queue::at(home.join("queue")), &home, &talks, &talk, request.operation,
                                    ).await.map_err(|e| format!("{e:#}"));
                                    // A client that stops reading must not hold the execution lock.
                                    drop(_guard);
                                    let response = serde_json::to_vec(&Response { id: request.id, result })?;
                                    ensure!(response.len() <= MAX_FRAME, "control response too large");
                                    stream.write_u32(response.len() as u32).await?;
                                    stream.write_all(&response).await?;
                                    Ok::<_, anyhow::Error>(())
                                }.await;
                            });
                        }
                        _ = clients.join_next(), if !clients.is_empty() => {}
                    }
                }
            }
        });
        Ok(Self {
            directory,
            token,
            active,
            server,
        })
    }

    pub(super) fn attach(&self, command: &mut tokio::process::Command) {
        command
            .env(SOCKET_ENV, self.directory.join("channel"))
            .env(CAP_ENV, &self.token);
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub(super) fn request(body: &[u8], id: u64) -> Result<String> {
    let path = std::env::var_os(SOCKET_ENV).context("control endpoint missing; backend closed inherited descriptors and did not preserve the socket environment").context(Unavailable)?;
    let token = std::env::var(CAP_ENV).context("missing control capability")?;
    request_at(Path::new(&path), &token, body, id)
}

fn request_at(path: &Path, token: &str, body: &[u8], id: u64) -> Result<String> {
    let mut stream = std::os::unix::net::UnixStream::connect(path)
        .context("control endpoint unavailable or expired (permission denied can indicate a sandbox refusal); repair launcher/backend transport without widening permissions")
        .context(Unavailable)?;
    ensure!(
        stream.peer_cred_compat()? == unsafe { libc::geteuid() },
        "control server UID mismatch"
    );
    ensure!(token.len() == 64, "invalid control capability");
    stream.set_read_timeout(Some(AUTH_TIMEOUT))?;
    stream.set_write_timeout(Some(AUTH_TIMEOUT))?;
    stream
        .write_all(token.as_bytes())
        .context("authenticate control channel")?;
    let mut ack = [0];
    stream
        .read_exact(&mut ack)
        .context("control authentication rejected or endpoint expired; no operation sent")?;
    ensure!(
        ack == *b"A",
        "control authentication rejected; no operation sent"
    );
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    (|| -> Result<Response> {
        stream.write_all(&(body.len() as u32).to_be_bytes())?;
        stream.write_all(body)?;
        receive(&mut stream, id)
    })().context("chat control operation may have executed; outcome is unknown; inspect its receipt before retrying")?.result.map_err(anyhow::Error::msg)
}

// Tokio exposes portable Unix peer credentials; duplicate the stream solely to
// query them without placing a blocking stream into the async reactor.
trait PeerCredentials {
    fn peer_cred_compat(&self) -> Result<libc::uid_t>;
}
impl PeerCredentials for std::os::unix::net::UnixStream {
    fn peer_cred_compat(&self) -> Result<libc::uid_t> {
        let copy = self.try_clone()?;
        copy.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(copy)?;
        let uid = stream.peer_cred()?.uid();
        // O_NONBLOCK belongs to the shared open-file description.
        self.set_nonblocking(false)?;
        Ok(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_directory_prefers_temp_and_falls_back_for_length_or_creation_failure() {
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let preferred = socket_directory([tmp.path().to_owned(), "/tmp".into()]).unwrap();
        assert_eq!(preferred.parent(), Some(tmp.path()));
        assert_eq!(
            std::fs::metadata(&preferred).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir(preferred).unwrap();

        for unusable in [tmp.path().join("x".repeat(108)), tmp.path().join("missing")] {
            let directory = socket_directory([unusable, tmp.path().to_owned()]).unwrap();
            assert_eq!(directory.parent(), Some(tmp.path()));
            let path = directory.join("channel");
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            drop(listener);
            std::fs::remove_dir_all(directory).unwrap();
        }
        assert!(socket_directory([tmp.path().join("missing")]).is_err());
    }

    async fn call(socket: &Socket, token: &str, operation: Operation) -> Result<String> {
        let path = socket.directory.join("channel");
        let token = token.to_owned();
        tokio::task::spawn_blocking(move || {
            let body = serde_json::to_vec(&Request { id: 1, operation })?;
            request_at(&path, &token, &body, 1)
        })
        .await?
    }

    #[tokio::test]
    async fn capabilities_cannot_cross_conversations_and_revocation_is_immediate() {
        use crate::queue::{Queue, Source, Task};
        let tmp = tempfile::tempdir().unwrap();
        let talks = crate::talk::Talks::at(tmp.path().join("talks"));
        let mut conversations = Vec::new();
        for id in ["first-chat", "second-chat"] {
            let mut talk: crate::talk::Talk = serde_json::from_value(serde_json::json!({
                "schema": 1, "id": id, "repo": ".", "agent": "mock",
                "status": "open", "turns": [], "created_at": jiff::Timestamp::now(),
                "updated_at": jiff::Timestamp::now(),
                "seat": crate::agent::SeatState::new("talk", "mock", 1)
            }))
            .unwrap();
            talks.put(&mut talk).unwrap();
            conversations.push(talk);
        }
        let queue = Queue::at(tmp.path().join("queue"));
        let mut task = Task::new(
            "work".into(),
            "work".into(),
            tmp.path().into(),
            Source::Human,
        );
        queue.put(&mut task).unwrap();
        let first = Socket::start(tmp.path(), &conversations[0].id, Serial::default()).unwrap();
        let second = Socket::start(tmp.path(), &conversations[1].id, Serial::default()).unwrap();
        let hold = || Operation::Hold {
            task: task.id.clone(),
            reason: "owner request".into(),
        };
        let before = std::fs::read(queue.path_of(&task.id)).unwrap();
        assert!(call(&first, &second.token, hold()).await.is_err());
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        assert!(call(&first, &first.token, hold()).await.is_ok());
        conversations[0].status = crate::talk::TalkStatus::Closed;
        talks.put(&mut conversations[0]).unwrap();
        let before = std::fs::read(queue.path_of(&task.id)).unwrap();
        assert!(
            call(
                &first,
                &first.token,
                Operation::Release {
                    task: task.id.clone()
                }
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read(queue.path_of(&task.id)).unwrap(), before);
        let path = second.directory.join("channel");
        let token = second.token.clone();
        drop(second);
        assert!(!path.exists());
        let error = tokio::task::spawn_blocking(move || request_at(&path, &token, b"{}", 1))
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.is::<Unavailable>());
    }
}
