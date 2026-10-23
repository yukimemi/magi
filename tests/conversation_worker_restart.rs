//! Chat executions retain their session while the HTTP process is replaced.
#![cfg(unix)]

mod common;

use std::fs::File;
#[cfg(not(target_os = "macos"))]
use std::io::Write;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Duration;

use common::{Judges, fixture, home_lock, magi_cli};
use magi::config::AgentKind;
use magi::talk::{Talks, Who};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Web(Child);
impl Web {
    fn stop(&mut self) {
        self.0.kill().expect("kill UI, not execution workers");
        self.0.wait().expect("reap UI");
    }
}
impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Gates {
    home: PathBuf,
    state: PathBuf,
}
impl Drop for Gates {
    fn drop(&mut self) {
        for number in [1, 2] {
            let _ = std::fs::write(self.state.join(format!("release-{number}")), "go");
        }
        // Do not leave a paid-invocation substitute running after a failed test.
        for _ in 0..100 {
            if magi::worker::active(&self.home)
                .is_ok_and(|jobs| jobs.iter().all(|j| j.kind != "chat"))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn executable(path: &Path, source: &str) {
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn generation_executable(path: &Path, label: &str) {
    std::fs::copy(env!("CARGO_BIN_EXE_magi"), path).expect("copy real generation executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    #[cfg(not(target_os = "macos"))]
    {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "\nconversation restart generation {label}").unwrap();
    }
    #[cfg(target_os = "macos")]
    {
        let signed = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--identifier"])
            .arg(format!("magi.conversation-test.{label}"))
            .arg(path)
            .output()
            .expect("ad-hoc sign test generation");
        assert!(
            signed.status.success(),
            "codesign: {}",
            String::from_utf8_lossy(&signed.stderr)
        );
    }
    let version = std::process::Command::new(path)
        .arg("--version")
        .output()
        .expect("launch real test generation");
    assert!(
        version.status.success(),
        "generation launch: {}",
        String::from_utf8_lossy(&version.stderr)
    );
}

async fn wait_for(mut predicate: impl FnMut() -> bool, what: &str) {
    tokio::time::timeout(Duration::from_secs(90), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn web(
    home: &Path,
    repo: &Path,
    generation: &Path,
    bin: &Path,
    log: &Path,
) -> (Web, SocketAddr) {
    std::fs::create_dir_all(home).unwrap();
    let output = File::create(log).unwrap();
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let child = magi_cli(home)
        .args(["web", "--bind", "127.0.0.1", "--port", "0", "--repo"])
        .arg(repo)
        .env("PATH", path)
        .env("MAGI_WORKER_EXE", generation)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .spawn()
        .expect("start real web process");
    let process = Web(child);
    let mut address = None;
    wait_for(
        || {
            address = std::fs::read_to_string(log).ok().and_then(|log| {
                let start = log.find("http://127.0.0.1:")? + "http://".len();
                let text: String = log[start..]
                    .chars()
                    .take_while(|ch| ch.is_ascii_digit() || matches!(ch, '.' | ':'))
                    .collect();
                text.parse().ok()
            });
            address.is_some()
        },
        "ephemeral HTTP listener",
    )
    .await;
    (process, address.unwrap())
}

async fn request(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let body = body.map(|value| value.to_string()).unwrap_or_default();
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").expect("HTTP response");
        let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
        let value = serde_json::from_str(body).unwrap_or_else(|error| panic!("invalid JSON: {error}: {response}"));
        (status, value)
    }).await.expect("HTTP request timeout")
}

const CLAUDE: &str = r#"#!/bin/sh
set -eu
session=
mode=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --session-id|--resume) mode="$1"; shift; session="$1" ;;
    esac
    shift
done
state="$CHAT_TEST_STATE"
if ! mkdir "$state/active" 2>/dev/null; then
    : > "$state/overlap"
    exit 1
fi
trap 'rmdir "$state/active"' EXIT
number=1
if [ -f "$state/count" ]; then number=$(($(cat "$state/count") + 1)); fi
printf '%s' "$number" > "$state/count"
printf '%s\t%s\t%s\t%s\t%s\n' "$number" "$mode" "$session" "$MAGI_SEAT" "$MAGI_WORKER_GENERATION" >> "$state/invocations"
: > "$state/started-$number"
while [ ! -f "$state/release-$number" ]; do sleep 0.05; done
printf '{"result":"response-%s","session_id":"%s","is_error":false}\n' "$number" "$session"
"#;

common::e2e! {
async fn chat_survives_web_restart_and_successor_submission_keeps_one_session() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let home = magi::run::home();
    let state = fx.tmp.path().join("chat-state");
    let bin = fx.tmp.path().join("bin");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let _gates = Gates { home: home.clone(), state: state.clone() };
    executable(&bin.join("claude"), CLAUDE);
    let mut chatter = fx.config.agents[0].clone();
    chatter.id = "chat-test".into();
    chatter.kind = AgentKind::Claude;
    chatter.command.clear();
    chatter.env.insert("CHAT_TEST_STATE".into(), state.display().to_string());
    fx.config.agents = vec![chatter];
    fx.config.daemon.max_deputies = 0;
    fx.config.daemon.release_stall_minutes = 0;
    fx.config.repos.fetch_interval = 0;
    std::fs::write(fx.repo.join("magi.toml"), toml::to_string(&fx.config).unwrap()).unwrap();
    for args in [
        vec!["add", "magi.toml"],
        vec!["commit", "-m", "configure isolated chat worker"],
        vec!["push", "origin", "main"],
        vec!["fetch", "origin"],
    ] {
        magi::git::git(&fx.repo, &args).await.expect("publish fixture configuration");
    }
    let old_executable = fx.tmp.path().join("old-worker");
    let new_executable = fx.tmp.path().join("new-worker");
    generation_executable(&old_executable, "old");
    generation_executable(&new_executable, "new");
    let (mut old, old_address) = web(&home, &fx.repo, &old_executable, &bin, &fx.tmp.path().join("old-web.log")).await;
    let (status, created) = request(old_address, "POST", "/api/talks", Some(json!({"agent": "chat-test"}))).await;
    assert_eq!(status, 201, "{created}");
    let id = created["id"].as_str().expect("talk id");
    let endpoint = format!("/api/talks/{id}");
    assert_eq!(request(old_address, "POST", &format!("{endpoint}/say"), Some(json!({"text": "first send"}))).await.0, 202);
    wait_for(|| state.join("started-1").exists(), "first response invocation").await;
    let original = magi::worker::active(&home).unwrap().into_iter().find(|j| j.kind == "chat").unwrap();
    assert_ne!(original.pid, Some(old.0.id()));
    assert_eq!(request(old_address, "POST", &format!("{endpoint}/say"),
        Some(json!({"text": "old queued send"}))).await.0, 202);
    let old_queued = magi::worker::active(&home).unwrap().into_iter()
        .find(|job| job.kind == "chat" && job.id != original.id)
        .expect("old-generation queued drainer");
    assert_eq!(old_queued.generation, original.generation);
    old.stop();
    let (_new, new_address) = web(&home, &fx.repo, &new_executable, &bin, &fx.tmp.path().join("new-web.log")).await;
    let (status, busy) = request(new_address, "GET", &endpoint, None).await;
    assert_eq!(status, 200);
    assert_eq!(busy["thinking"], true, "replacement UI must see old worker lease");
    let (status, queued) = request(new_address, "POST", &format!("{endpoint}/say"), Some(json!({"text": "new queued send"}))).await;
    assert_eq!(status, 202, "{queued}");
    assert!(!state.join("started-2").exists(), "queued worker must wait for the first session turn");
    let successor = magi::worker::active(&home).unwrap().into_iter()
        .find(|job| job.kind == "chat" && job.generation != original.generation)
        .expect("durable successor-generation queued worker");
    assert_ne!(successor.generation, original.generation, "newly submitted work uses successor executable");
    assert!(successor.sequence > old_queued.sequence, "reservation order is durable, not clock-derived");
    std::fs::write(state.join("release-1"), "go").unwrap();
    wait_for(|| state.join("started-2").exists(), "queued response after the old invocation").await;
    let talks = Talks::at(home.join("talks"));
    assert!(talks.get(id).unwrap().turns.iter().any(|turn| turn.body == "response-1"));
    std::fs::write(state.join("release-2"), "go").unwrap();
    wait_for(|| talks.get(id).unwrap().turns.iter().any(|turn| turn.body == "response-2")
        && !magi::consult::turn_worker_active(&talks, id), "both persisted responses").await;
    let talk = talks.get(id).unwrap();
    assert!(talk.pending.is_empty());
    let authors: Vec<_> = talk.turns.iter().map(|turn| turn.who).collect();
    assert_eq!(authors, vec![Who::Operator, Who::Agent, Who::Operator, Who::Agent]);
    let owner_sends: Vec<_> = talk.turns.iter().filter(|turn| turn.who == Who::Operator)
        .flat_map(|turn| {
            if turn.messages.is_empty() {
                vec![turn.body.as_str()]
            } else {
                turn.messages.iter().map(|message| message.body.as_str()).collect()
            }
        }).collect();
    assert_eq!(owner_sends, vec!["first send", "old queued send", "new queued send"]);
    let replies: Vec<_> = talk.turns.iter().filter(|turn| turn.who == Who::Agent)
        .map(|turn| turn.body.as_str()).collect();
    assert_eq!(replies, vec!["response-1", "response-2"]);
    let invocations = std::fs::read_to_string(state.join("invocations")).unwrap();
    let rows: Vec<Vec<_>> = invocations.lines().map(|line| line.split('\t').collect()).collect();
    assert_eq!(rows.len(), 2, "the queued burst is delivered exactly once, by the successor");
    assert_eq!(rows[0][1], "--session-id");
    assert_eq!(rows[1][1], "--resume");
    assert_eq!(rows[0][2], rows[1][2], "same CLI session after UI replacement");
    assert_eq!(rows[0][3], rows[1][3], "same talk seat");
    assert_eq!(rows[0][4], original.generation);
    assert_eq!(rows[1][4], successor.generation);
    assert!(!state.join("overlap").exists());
    assert_eq!(request(new_address, "GET", &endpoint, None).await.1["thinking"], false);
}
}
