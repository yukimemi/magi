//! Real CLI + detached worker round trips against an isolated deterministic backend.
#[allow(dead_code)]
#[path = "web_render/cdp.rs"]
mod cdp;
#[allow(unused_macros, unused_imports)]
mod common;
#[allow(dead_code)]
#[path = "web_render/diag.rs"]
mod diag;

use magi::{
    config::{AgentKind, AgentSpec, Config},
    talk::{self, Talks},
};
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;

struct Fixture {
    tmp: tempfile::TempDir,
    talks: Talks,
    id: String,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let cli = common::magi_cli(tmp.path());
        let binary = cli.get_program().to_string_lossy().replace('\\', "/");
        let script = repo.join("backend.sh");
        std::fs::write(
            &script,
            r#"set -eu
cp "$MAGI_PROMPT_FILE" input.txt
printf 'call\n' >> calls.txt
if [ "$MAGI_NODE" = external-chat ]; then
  if "$TEST_MAGI" task control hold nonexistent > control.out 2>&1; then exit 41; fi
  grep -q 'no owner authority' control.out
fi
: > entered
while [ ! -f release ]; do sleep 0.05; done
printf 'correlated reply\n'
"#,
        )
        .unwrap();
        let cfg = Config {
            agents: vec![AgentSpec {
                id: "mock".into(),
                kind: AgentKind::Command,
                model: None,
                command: vec!["sh".into(), script.to_string_lossy().replace('\\', "/")],
                extra_args: vec![],
                env: std::collections::BTreeMap::from([("TEST_MAGI".into(), binary)]),
                prompt_delivery: None,
                max_turn_secs: Some(30),
            }],
            ..Config::default()
        };
        std::fs::write(repo.join("magi.toml"), toml::to_string(&cfg).unwrap()).unwrap();
        let talks = Talks::at(tmp.path().join("talks"));
        let t = talk::begin(&talks, &cfg, repo, Some("mock")).unwrap();
        Self {
            tmp,
            talks,
            id: t.id,
        }
    }
    fn cli(&self, args: &[&str], stdin: Option<&str>) -> Value {
        let mut command = common::magi_cli(self.tmp.path());
        command
            .args(["talk"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        // Capture through real pipes, but bound EOF as well as process exit:
        // detached Windows children must not retain the caller's descriptors.
        let out = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime.block_on(async {
                        use tokio::io::AsyncWriteExt as _;
                        let mut command = tokio::process::Command::from(command);
                        command.kill_on_drop(true);
                        let mut child = command.spawn().unwrap();
                        if let Some(text) = stdin {
                            let mut input = child.stdin.take().unwrap();
                            input.write_all(text.as_bytes()).await.unwrap();
                            input.shutdown().await.unwrap();
                        }
                        tokio::time::timeout(Duration::from_secs(45), child.wait_with_output())
                            .await
                            .expect("CLI output/EOF did not complete within 45s")
                            .unwrap()
                    })
                })
                .join()
                .unwrap()
        });
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
    }
    fn send(&self, request: &str, text: &str) -> Value {
        self.cli(
            &[
                "send",
                &self.id,
                "--request",
                request,
                "--sender",
                "fixture-agent",
                "--json",
                "-",
            ],
            Some(text),
        )
    }
    fn query(&self, request: &str, wait: &str) -> Value {
        self.cli(
            &[
                "reply",
                &self.id,
                "--request",
                request,
                "--wait",
                wait,
                "--json",
            ],
            None,
        )
    }
    fn repo(&self) -> std::path::PathBuf {
        self.talks.get(&self.id).unwrap().repo
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Release the deterministic backend even when an assertion unwinds.
        let _ = std::fs::write(self.tmp.path().join("repo/release"), "");
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let live = magi::worker::active(self.tmp.path())
                .unwrap_or_default()
                .iter()
                .any(|job| job.payload["talk"].as_str() == Some(&self.id));
            if !live {
                break;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("fixture worker did not settle before cleanup");
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[tokio::test]
async fn external_cli_round_trip_is_correlated_idempotent_and_web_visible() {
    let _guard = common::home_lock().await;
    let f = Fixture::new();
    let discovered = f.cli(&["list", "--json"], None);
    assert_eq!(discovered["talks"][0]["id"], f.id);
    let sent = f.send("request-one", "first line\nsecond line");
    assert!(
        matches!(sent["status"].as_str(), Some("accepted" | "started")),
        "{sent}"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !f.repo().join("entered").exists() {
        let state = f.query("request-one", "0");
        assert!(
            std::time::Instant::now() < deadline && state["status"] != "failed",
            "backend did not start: {state}; stderr: {}; control: {}",
            std::fs::read_to_string(f.talks.artifacts_of(&f.id).join("turn-1.err"))
                .unwrap_or_default(),
            std::fs::read_to_string(f.repo().join("control.out")).unwrap_or_default(),
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(f.query("request-one", "1")["status"], "timeout");
    assert_eq!(f.send("request-one", "different")["status"], "conflict");
    assert_eq!(f.send("request-two", "later")["status"], "busy");
    let replay = f.send("request-one", "first line\nsecond line");
    assert_eq!(replay["input_id"], sent["input_id"]);
    let mut t = f.talks.get(&f.id).unwrap();
    talk::queue(&mut t, &f.talks, "unsent owner draft", vec![]).unwrap();
    talk::post(&f.talks, &f.id, "unrelated task post", "another-task").unwrap();
    std::fs::write(f.repo().join("release"), "").unwrap();
    let reply = f.query("request-one", "30");
    assert_eq!(reply["status"], "replied", "{reply}");
    assert_eq!(reply["reply"], "correlated reply");
    assert_ne!(reply["reply_id"], reply["input_id"]);
    let saved = f.talks.get(&f.id).unwrap();
    assert_eq!(
        saved.pending, "unsent owner draft",
        "external worker must not drain owner input"
    );
    assert_eq!(saved.turns[0].sender, Some(talk::Sender::External));
    let input = std::fs::read_to_string(f.repo().join("input.txt")).unwrap();
    assert!(
        input.contains("external") && input.contains("not") && input.contains("owner"),
        "{input}"
    );
    assert!(input.contains("first line") && input.contains("second line"));
    assert!(saved.turns[0].body.contains("first line\nsecond line"));
    assert_eq!(
        f.send("request-one", "first line\nsecond line")["reply_id"],
        reply["reply_id"]
    );
    assert_eq!(
        std::fs::read_to_string(f.repo().join("calls.txt")).unwrap(),
        "call\n"
    );
    let history = f.cli(
        &[
            "history",
            &f.id,
            "--after",
            sent["input_id"].as_str().unwrap(),
            "--json",
        ],
        None,
    );
    assert_eq!(
        history["messages"].as_array().unwrap().last().unwrap()["id"],
        reply["reply_id"]
    );
    assert_eq!(history["messages"][0]["body"], "unrelated task post");
    // A newly constructed CLI reads the same receipt after the sender exits.
    assert_eq!(f.query("request-one", "0")["reply_id"], reply["reply_id"]);

    if let Some(chrome) = cdp::find_chrome() {
        let home = f.tmp.path();
        let ui = magi::web::Ui::new(
            magi::queue::Queue::at(home.join("queue")),
            magi::ask::Questions::at(home.join("questions")),
            f.talks.clone(),
            home.join("runs"),
            home.to_path_buf(),
            f.repo(),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let url = format!("http://{}/#/chat/{}", listener.local_addr().unwrap(), f.id);
        let server = tokio::spawn(async move {
            axum::serve(listener, ui.router()).await.unwrap();
        });
        let mut browser = cdp::Browser::launch(&chrome).await.unwrap();
        let page = browser.open_page(&url, 1280, 900, false).await.unwrap();
        browser
            .wait_for(
                &page,
                "document.querySelector('#talk-turns')?.textContent.includes('correlated reply')",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
        let visible = browser.eval(&page,"(() => { const e = [...document.querySelectorAll('#talk-turns .turn')].find(e=>e.textContent.includes('first line')); return {text:e?.textContent, avatar:e?.querySelector('img')?.getAttribute('src'), who:e?.dataset.who}; })()").await.unwrap();
        assert!(
            visible["text"].as_str().unwrap().contains("External"),
            "{visible}"
        );
        assert!(
            !visible["avatar"]
                .as_str()
                .unwrap_or("")
                .contains("operator.svg"),
            "{visible}"
        );
        assert_ne!(visible["who"], "operator");
        browser.close_page(&page).await;
        drop(browser);
        server.abort();
        let _ = server.await;
    } else {
        assert!(std::env::var_os("CI").is_none(), "Chrome is required in CI");
        eprintln!("SKIP external web visibility: no Chrome");
    }
}

#[tokio::test]
async fn external_admission_preserves_drafts_and_uncertain_occurrences() {
    let _guard = common::home_lock().await;
    let f = Fixture::new();
    let mut t = f.talks.get(&f.id).unwrap();
    talk::queue(&mut t, &f.talks, "owner draft", vec![]).unwrap();
    assert_eq!(f.send("draft-refusal", "message")["status"], "busy");
    assert!(f.talks.get(&f.id).unwrap().turns.is_empty());
    talk::clear_pending(&mut t, &f.talks).unwrap();
    // Accepted ledger before reservation publication: a crash is uncertain,
    // and explicit replay may only observe, never invent non-execution proof.
    talk::record(&mut t, &f.talks, "message", vec![]).unwrap();
    t.turns[0].sender = Some(talk::Sender::External);
    t.turns[0].messages.clear();
    t.external_requests.push(talk::external::Request {
        request: "lost".into(),
        sender: "fixture-agent".into(),
        text: "message".into(),
        input_turn: 0,
        reply_turn: None,
        status: "accepted".into(),
        worker: None,
        error: None,
    });
    f.talks.put(&mut t).unwrap();
    assert_eq!(f.send("lost", "message")["status"], "uncertain");
    assert_eq!(f.query("lost", "0")["status"], "uncertain");
    assert!(!f.repo().join("calls.txt").exists());
    assert_eq!(f.send("lost", "other")["status"], "conflict");
    talk::close(&mut t, &f.talks).unwrap();
    assert_eq!(f.send("new", "message")["status"], "closed");
    assert_eq!(
        f.cli(&["history", "absent", "--json"], None)["status"],
        "not_found"
    );
    let mut second = t.clone();
    second.id = format!("{}-other", f.id);
    f.talks.create(&mut second).unwrap();
    assert_eq!(
        f.cli(&["history", &f.id[..8], "--json"], None)["status"],
        "ambiguous"
    );
    assert!(talk::external::history(&f.talks, &f.id, Some("another-talk:0:0")).is_err());
    // Started-but-lost worker receipts likewise cannot be replayed.
    let mut t = f.talks.get(&f.id).unwrap();
    t.external_requests[0].status = "started".into();
    t.external_requests[0].worker = Some("00000000-0000-0000-0000-000000000000".into());
    f.talks.put(&mut t).unwrap();
    assert_eq!(f.query("lost", "0")["status"], json!("uncertain"));
}

#[tokio::test]
async fn external_worker_unavailability_keeps_the_request_without_starting_a_draft() {
    let _guard = common::home_lock().await;
    let f = Fixture::new();
    let output = common::magi_cli(f.tmp.path())
        .args([
            "talk",
            "send",
            &f.id,
            "--request",
            "no-worker",
            "--sender",
            "fixture-agent",
            "--json",
            "message",
        ])
        .env("MAGI_WORKER_EXE", f.tmp.path().join("missing-executable"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "not_started", "{result}");
    assert_eq!(f.send("no-worker", "message")["status"], "not_started");
    let saved = f.talks.get(&f.id).unwrap();
    assert_eq!(saved.turns[0].sender, Some(talk::Sender::External));
    assert!(saved.pending.is_empty());
    assert!(!f.repo().join("entered").exists());
}
