//! A deliberately small Chrome DevTools Protocol client, and the launcher that
//! goes with it.
//!
//! Only what `web_render` needs: start one headless Chrome, open a page, send
//! commands, collect events, evaluate an expression. A failed launch is retried, but there are no
//! reconnects; every wait carries a deadline so a wedged browser fails the test
//! instead of hanging CI.

use std::path::{Path, PathBuf};

use super::diag;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::Command;

#[path = "process.rs"]
mod process;
use process::Chrome;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// How long one command may take to answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a fresh Chrome may take to publish its debugging port.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);
/// How many times a failed launch is tried in all.
const LAUNCH_ATTEMPTS: usize = 3;
/// How much of Chrome's stderr a failed launch reports.
const STDERR_LINES: usize = 20;
/// Upper bound on the bytes of that tail.
const STDERR_BYTES: usize = 4096;

/// Where a Chrome/Chromium binary is, if this machine has one.
///
/// `MAGI_CHROME` wins, then the usual names on `PATH`, then the default
/// install locations on macOS and Windows.
pub fn find_chrome() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MAGI_CHROME").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
        "chrome.exe",
    ] {
        if let Some(p) = on_path(name) {
            return Some(p);
        }
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|p| p.is_file())
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One running headless Chrome and its browser-level DevTools connection.
///
/// Dropping it terminates and reaps the owned Chrome process tree before
/// removing the profile, including on failed assertions and failed launches.
pub struct Browser<S = MaybeTlsStream<TcpStream>> {
    child: Chrome,
    profile: tempfile::TempDir,
    ws: WebSocketStream<S>,
    next_id: u64,
    test: String,
    requests: Option<diag::Requests>,
    heartbeat: diag::Heartbeat,
    event_trace: std::collections::VecDeque<Value>,
    /// Events that arrived while a command was waiting for its answer, as
    /// `(sessionId, message)`.
    events: Vec<(Option<String>, Value)>,
}

impl Browser {
    /// Start Chrome, trying up to `LAUNCH_ATTEMPTS` times. Every attempt gets a
    /// fresh profile directory and the failed child is reaped first, so a
    /// cold start that is merely slow on a loaded runner does not fail the
    /// test, and one that keeps failing says why.
    pub async fn launch(chrome: &Path) -> Result<Self, String> {
        retry_launch(LAUNCH_ATTEMPTS, |_| Self::launch_once(chrome)).await
    }

    async fn launch_once(chrome: &Path) -> Result<Self, String> {
        let profile = tempfile::tempdir().map_err(|e| e.to_string())?;
        // stderr goes to a file, not a pipe: nobody reads a pipe while Chrome
        // starts, and a full one would block it.
        let log_path = profile.path().join("chrome-stderr.log");
        let log = std::fs::File::create(&log_path).map_err(|e| format!("create log: {e}"))?;
        let mut cmd = Command::new(chrome);
        cmd.arg("--headless=new")
            // CI runners run as root in a container; Chrome refuses to start
            // its sandbox there.
            .arg("--no-sandbox")
            .arg("--disable-gpu")
            .arg("--disable-dev-shm-usage")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-extensions")
            .arg("--hide-scrollbars")
            .arg("--enable-logging")
            .arg(format!("--log-file={}", profile.path().join("chrome-debug.log").display()))
            .arg("--vmodule=render_process_host_impl=1,child_process_launcher_helper=1,navigation_request=1")
            // Port 0: the OS picks, Chrome reports it in DevToolsActivePort.
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(log))
            .kill_on_drop(true);
        let mut child =
            Chrome::spawn(&mut cmd).map_err(|e| format!("start {}: {e}", chrome.display()))?;
        // `cmd` still holds the log handle; drop it so the file can be removed
        // with the profile on Windows once Chrome is gone.
        drop(cmd);

        let outcome = Self::connect(&mut child, profile.path()).await;
        match outcome {
            Ok(ws) => Ok(Self {
                child,
                profile,
                ws,
                next_id: 0,
                test: std::thread::current()
                    .name()
                    .unwrap_or("unnamed")
                    .to_owned(),
                requests: None,
                heartbeat: diag::Heartbeat::start(),
                event_trace: Default::default(),
                events: Vec::new(),
            }),
            Err(why) => {
                drop(child);
                let stderr = std::fs::read_to_string(&log_path).unwrap_or_default();
                let mut msg = why;
                let tail = tail_lines(&stderr, STDERR_LINES);
                if tail.trim().is_empty() {
                    msg.push_str("; Chrome wrote nothing to stderr");
                } else {
                    msg.push_str("; Chrome stderr tail: ");
                    msg.push_str(&tail);
                }
                drop(profile);
                Err(msg)
            }
        }
    }

    /// Wait for the debugging port and open the browser-level connection.
    async fn connect(child: &mut Chrome, profile: &Path) -> Result<Socket, String> {
        let marker = profile.join("DevToolsActivePort");
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        let url = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    break format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                }
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!(
                    "Chrome exited with {status} before publishing DevToolsActivePort"
                ));
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "Chrome did not publish DevToolsActivePort within {LAUNCH_TIMEOUT:?}"
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let (ws, _) = tokio::time::timeout(CALL_TIMEOUT, connect_async(url.as_str()))
            .await
            .map_err(|_| "DevTools connect timed out".to_owned())?
            .map_err(|e| format!("DevTools connect: {e}"))?;
        Ok(ws)
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> Browser<S> {
    /// Send one command and wait for its answer, keeping any events that
    /// arrive meanwhile. `session` addresses a page target.
    pub async fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.call_within(session, method, params, CALL_TIMEOUT)
            .await
    }

    async fn call_within(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
        within: Duration,
    ) -> Result<Value, String> {
        let started_ms = diag::now();
        match tokio::time::timeout(within, self.call_inner(session, method, params)).await {
            Ok(result) => result,
            Err(_) => {
                let evidence = self.diagnostics(session).await;
                Err(format!(
                    "{method}: no answer within {within:?}; started_ms={started_ms}; {evidence}"
                ))
            }
        }
    }

    // Also bound socket writes. Diagnostic probes use this directly so they
    // cannot recursively diagnose themselves or extend the failed command.
    async fn call_inner(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.ws
            .send(Message::text(msg.to_string()))
            .await
            .map_err(|e| format!("{method}: send: {e}"))?;
        loop {
            let Some(v) = self.read(CALL_TIMEOUT).await? else {
                continue;
            };
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(format!("{method}: {err}"));
                }
                return Ok(v["result"].clone());
            }
        }
    }

    // Preserve evidence before cleanup deletes the profile. Probes address the
    // browser, not the unresponsive page, and share one six-second deadline.
    async fn diagnostics(&mut self, session: Option<&str>) -> String {
        let at = diag::now();
        let process = match self.child.try_wait() {
            Ok(Some(status)) => format!("exited: {status}"),
            Ok(None) => "running".to_owned(),
            Err(error) => format!("status unavailable: {error}"),
        };
        let pid = self.child.id();
        let before = json!({"at_ms": at, "test": self.test, "browser_pid": pid,
            "Chrome": process, "heartbeat": self.heartbeat.snapshot(),
            "requests": self.requests.as_ref().map(diag::Requests::snapshot),
            "events": self.event_trace.iter().filter(|event|
                event["event"]["sessionId"].as_str().is_none_or(|s| Some(s) == session))
                .cloned().collect::<Vec<_>>()});
        // An OS query cannot be cancelled. Keep it off Tokio's blocking pool:
        // runtime shutdown otherwise waits forever for a stuck diagnostic query.
        let (sender, os) = tokio::sync::oneshot::channel();
        let sampler = std::thread::Builder::new()
            .name("chrome-diagnostics".to_owned())
            .spawn(move || {
                let _ = sender.send(diag::os_samples(pid.unwrap_or(0)));
            });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        let mut probes = Vec::new();
        for method in [
            "SystemInfo.getProcessInfo",
            "Browser.getVersion",
            "Target.getTargets",
            "SystemInfo.getProcessInfo",
        ] {
            if method == "SystemInfo.getProcessInfo" && !probes.is_empty() {
                tokio::time::sleep_until(
                    (tokio::time::Instant::now() + Duration::from_secs(1)).min(deadline),
                )
                .await;
            }
            let end = (tokio::time::Instant::now() + Duration::from_secs(1)).min(deadline);
            let result =
                tokio::time::timeout_at(end, self.call_inner(None, method, json!({}))).await;
            probes.push(json!({"at_ms": diag::now(), "method": method,
            "result": match result {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => json!({"error": error}),
                Err(_) => json!({"error": "diagnostic probe timed out"}),
            }}));
        }
        let os = match tokio::time::timeout_at(deadline, os).await {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                json!({"error": format!("OS sampler disconnected: {error}; spawn: {sampler:?}")})
            }
            Err(_) => json!({"error": "OS sampling exceeded diagnostic deadline"}),
        };
        format!(
            "{before}\n browser probes: {}\n OS samples: {os}\n HTTP after probes: {}\n Chrome stderr tail: {}\n Chrome debug tail: {}",
            json!(probes),
            self.requests
                .as_ref()
                .map(diag::Requests::snapshot)
                .unwrap_or(Value::Null),
            file_tail(&self.profile.path().join("chrome-stderr.log")),
            file_tail(&self.profile.path().join("chrome-debug.log"))
        )
    }

    /// Read one message within `within`. Events are stored; the first
    /// non-event (a command answer) is returned. `None` on timeout.
    async fn read(&mut self, within: Duration) -> Result<Option<Value>, String> {
        let next = match tokio::time::timeout(within, self.ws.next()).await {
            Err(_) => return Ok(None),
            Ok(n) => n,
        };
        let msg = match next {
            None => return Err("DevTools connection closed".to_owned()),
            Some(Err(e)) => return Err(format!("DevTools read: {e}")),
            Some(Ok(m)) => m,
        };
        let Message::Text(text) = msg else {
            return Ok(Some(Value::Null));
        };
        let v: Value = serde_json::from_str(text.as_str()).map_err(|e| e.to_string())?;
        if v.get("id").is_none() {
            let session = v
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if self.event_trace.len() == 40 {
                self.event_trace.pop_front();
            }
            self.event_trace
                .push_back(json!({"at_ms": diag::now(), "event": v}));
            self.events.push((session, v));
            return Ok(Some(Value::Null));
        }
        Ok(Some(v))
    }

    /// Keep reading for `dur` so events that are still in flight land.
    pub async fn settle(&mut self, dur: Duration) -> Result<(), String> {
        let end = Instant::now() + dur;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || self.read(left).await?.is_none() {
                return Ok(());
            }
        }
    }

    /// Take every event recorded for `session` so far.
    pub fn take_events(&mut self, session: &str) -> Vec<Value> {
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.events)
            .into_iter()
            .partition(|(s, _)| s.as_deref() == Some(session));
        self.events = rest;
        mine.into_iter().map(|(_, v)| v).collect()
    }

    /// A fresh page with the given viewport, already navigated to `url`, with
    /// the domains the harness listens to switched on first.
    pub async fn open_page(
        &mut self,
        url: &str,
        width: u32,
        height: u32,
        mobile: bool,
    ) -> Result<Page, String> {
        self.requests = diag::Requests::for_url(url);
        let t = self
            .call(None, "Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        let target_id = t["targetId"].as_str().ok_or("no targetId")?.to_owned();
        let a = self
            .call(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session = a["sessionId"].as_str().ok_or("no sessionId")?.to_owned();
        let s = Some(session.as_str());
        self.call(s, "Page.enable", json!({})).await?;
        self.call(s, "Runtime.enable", json!({})).await?;
        self.call(s, "Log.enable", json!({})).await?;
        self.call(
            s,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width, "height": height,
                "deviceScaleFactor": 1, "mobile": mobile,
            }),
        )
        .await?;
        self.call(s, "Page.navigate", json!({ "url": url })).await?;
        Ok(Page { session, target_id })
    }

    pub async fn close_page(&mut self, page: &Page) {
        let _ = self
            .call(
                None,
                "Target.closeTarget",
                json!({ "targetId": page.target_id }),
            )
            .await;
    }

    /// Evaluate `expression` in the page and return its JSON value.
    pub async fn eval(&mut self, page: &Page, expression: &str) -> Result<Value, String> {
        let r = self
            .call(
                Some(&page.session),
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                }),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            return Err(format!("evaluate threw: {ex}"));
        }
        Ok(r["result"]["value"].clone())
    }

    /// Poll `predicate` (a JS expression) until it is truthy or `within`
    /// passes. A fixed sleep would be a guess; the SSE stream keeps the
    /// network busy forever, so "network idle" is not available either.
    pub async fn wait_for(
        &mut self,
        page: &Page,
        predicate: &str,
        within: Duration,
    ) -> Result<(), String> {
        let end = Instant::now() + within;
        loop {
            if self.eval(page, predicate).await?.as_bool() == Some(true) {
                return Ok(());
            }
            if Instant::now() > end {
                return Err(format!("timed out waiting for: {predicate}"));
            }
            self.settle(Duration::from_millis(100)).await?;
        }
    }
}

/// An attached page target.
pub struct Page {
    pub session: String,
    target_id: String,
}

/// Run `attempt(n)` (1-based) until it succeeds or `attempts` runs are spent.
/// All failures are kept, in order, in the final error.
async fn retry_launch<T, F, Fut>(attempts: usize, mut attempt: F) -> Result<T, String>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut failures = Vec::new();
    for n in 1..=attempts {
        match attempt(n).await {
            Ok(v) => return Ok(v),
            Err(e) => failures.push(format!("attempt {n}/{attempts}: {e}")),
        }
    }
    Err(failures.join("; "))
}

// Read only a bounded tail, even if verbose Chrome logging grew a large file.
fn file_tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let read = || -> std::io::Result<String> {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(16 * 1024)))?;
        let mut bytes = Vec::new();
        file.take(16 * 1024).read_to_end(&mut bytes)?;
        Ok(tail_lines(&String::from_utf8_lossy(&bytes), 80))
    };
    read().unwrap_or_else(|error| format!("unavailable: {error}"))
}

/// The last `n` lines of `text`, also capped to `STDERR_BYTES` from the end.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let joined = lines[lines.len().saturating_sub(n)..].join("\n");
    if joined.len() <= STDERR_BYTES {
        return joined;
    }
    let mut start = joined.len() - STDERR_BYTES;
    while !joined.is_char_boundary(start) {
        start += 1;
    }
    joined[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exercise_timeout(respond: bool) -> String {
        use tokio_tungstenite::tungstenite::protocol::Role;
        let (client, server) = tokio::io::duplex(16 * 1024);
        let server = tokio::spawn(async move {
            let mut ws = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
            let mut methods = Vec::new();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let command: Value = serde_json::from_str(&text).unwrap();
                let method = command["method"].as_str().unwrap().to_owned();
                methods.push(method.clone());
                if method == "Page.navigate" {
                    continue;
                }
                if !respond {
                    if methods.len() == 5 {
                        // Keep the socket open through the final probe timeout.
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        break;
                    }
                    continue;
                }
                // Include the original late answer: it must not satisfy a probe.
                ws.send(Message::text(
                    json!({"id": 1, "result": {"late": true}}).to_string(),
                ))
                .await
                .unwrap();
                ws.send(Message::text(
                    json!({"id": command["id"], "result": {"probe": method}}).to_string(),
                ))
                .await
                .unwrap();
                if methods.len() == 5 {
                    break;
                }
            }
            methods
        });
        let ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let profile = tempfile::tempdir().unwrap();
        let child = Chrome::spawn(&mut process::tests::helper_command(
            &profile.path().join("leaf"),
        ))
        .unwrap();
        let mut browser = Browser {
            child,
            profile,
            ws,
            next_id: 0,
            test: "fake-CDP".to_owned(),
            requests: None,
            heartbeat: diag::Heartbeat::start(),
            event_trace: Default::default(),
            events: Vec::new(),
        };
        let started = Instant::now();
        let error = browser
            .call_within(
                Some("page"),
                "Page.navigate",
                json!({}),
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(error.starts_with("Page.navigate: no answer within 20ms"));
        assert!(error.contains("fake-CDP"));
        assert!(error.contains("cpu_ms"));
        assert_eq!(
            server.await.unwrap(),
            [
                "Page.navigate",
                "SystemInfo.getProcessInfo",
                "Browser.getVersion",
                "Target.getTargets",
                "SystemInfo.getProcessInfo"
            ]
        );
        error
    }

    #[tokio::test]
    async fn timeout_probes_browser_without_retrying_page_command() {
        let error = exercise_timeout(true).await;
        assert!(error.contains("\"probe\":\"SystemInfo.getProcessInfo\""));
    }

    #[tokio::test]
    async fn unresponsive_browser_probes_are_bounded_and_do_not_recurse() {
        let error = exercise_timeout(false).await;
        assert_eq!(error.matches("diagnostic probe timed out").count(), 4);
        assert_eq!(error.matches("browser probes:").count(), 1);
    }

    #[tokio::test]
    async fn retry_stops_at_the_first_success() {
        let mut calls = 0;
        let got = retry_launch(3, |n| {
            calls += 1;
            async move { if n < 3 { Err(format!("e{n}")) } else { Ok(n) } }
        })
        .await;
        assert_eq!(got, Ok(3));
        assert_eq!(calls, 3);
        let mut calls = 0;
        let got = retry_launch(3, |n| {
            calls += 1;
            async move { Ok::<_, String>(n) }
        })
        .await;
        assert_eq!((got, calls), (Ok(1), 1));
    }

    #[tokio::test]
    async fn retry_reports_every_failure_and_stops_at_the_bound() {
        let mut calls = 0;
        let got: Result<(), String> = retry_launch(3, |n| {
            calls += 1;
            async move { Err(format!("e{n}")) }
        })
        .await;
        assert_eq!(calls, 3);
        assert_eq!(
            got.unwrap_err(),
            "attempt 1/3: e1; attempt 2/3: e2; attempt 3/3: e3"
        );
    }

    #[test]
    fn tail_lines_keeps_the_end() {
        assert_eq!(tail_lines("", 3), "");
        assert_eq!(tail_lines("a\nb", 3), "a\nb");
        assert_eq!(tail_lines("a\nb\nc\nd", 2), "c\nd");
        let long = "é".repeat(STDERR_BYTES);
        assert!(tail_lines(&long, 1).len() <= STDERR_BYTES);
    }
}
