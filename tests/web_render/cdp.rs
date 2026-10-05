//! A deliberately small Chrome DevTools Protocol client, and the launcher that
//! goes with it.
//!
//! Only what `web_render` needs: start one headless Chrome, open a page, send
//! commands, collect events, evaluate an expression. No retries and no
//! reconnects; every wait carries a deadline so a wedged browser fails the test
//! instead of hanging CI.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// How long one command may take to answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a fresh Chrome may take to publish its debugging port.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

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
/// Dropping it kills the process (`kill_on_drop`), so a failed assertion cannot
/// leave a browser behind.
pub struct Browser {
    _child: Child,
    _profile: tempfile::TempDir,
    ws: Socket,
    next_id: u64,
    /// Events that arrived while a command was waiting for its answer, as
    /// `(sessionId, message)`.
    events: Vec<(Option<String>, Value)>,
}

impl Browser {
    pub async fn launch(chrome: &Path) -> Result<Self, String> {
        let profile = tempfile::tempdir().map_err(|e| e.to_string())?;
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
            // Port 0: the OS picks, Chrome reports it in DevToolsActivePort.
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| format!("start {}: {e}", chrome.display()))?;

        let marker = profile.path().join("DevToolsActivePort");
        let deadline = Instant::now() + LAUNCH_TIMEOUT;
        let url = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    break format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                }
            }
            if Instant::now() > deadline {
                return Err("Chrome did not publish DevToolsActivePort in time".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let (ws, _) = tokio::time::timeout(CALL_TIMEOUT, connect_async(url.as_str()))
            .await
            .map_err(|_| "DevTools connect timed out".to_owned())?
            .map_err(|e| format!("DevTools connect: {e}"))?;
        Ok(Self {
            _child: child,
            _profile: profile,
            ws,
            next_id: 0,
            events: Vec::new(),
        })
    }

    /// Send one command and wait for its answer, keeping any events that
    /// arrive meanwhile. `session` addresses a page target.
    pub async fn call(
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
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(v) = self.read(left).await? else {
                return Err(format!("{method}: no answer within {CALL_TIMEOUT:?}"));
            };
            if v.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(format!("{method}: {err}"));
                }
                return Ok(v["result"].clone());
            }
        }
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
