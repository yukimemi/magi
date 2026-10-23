//! Failure-only evidence for Chrome stalls. All wall times are UTC epoch ms;
//! test names and browser pids correlate two concurrently failing tests.
//!
//! Read the evidence in layers: no HTTP arrival points before the server;
//! arrival without headers points at the handler; headers without body EOF
//! points at body delivery (an open SSE body is expected). A completed document
//! without frameNavigated points after HTTP delivery, not at a blocked handler.
//! Browser probes answering while page commands stall isolate the page/renderer
//! path. Missing renderers in both CDP and OS snapshots suggests startup trouble;
//! renderer CPU deltas distinguish activity from waiting, but zero CPU alone
//! does not prove deadlock. OS state, Chrome logs and runtime heartbeat gaps
//! supply the remaining context. Body EOF means the HTTP server consumed the
//! application stream, not that Chrome acknowledged the bytes. No probe answer
//! is evidence too, not a retry.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use futures_util::StreamExt;
use serde_json::{Value, json};

const REQUESTS: usize = 256;
static SERVERS: LazyLock<Mutex<BTreeMap<String, Weak<Mutex<Trace>>>>> =
    LazyLock::new(Mutex::default);

pub fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Default)]
pub struct Trace {
    next: u64,
    rows: VecDeque<Value>,
}

#[derive(Clone)]
pub struct Requests(Arc<Mutex<Trace>>);

impl Requests {
    pub fn register(base: &str) -> Self {
        let trace = Self(Arc::default());
        let mut servers = SERVERS.lock().unwrap();
        servers.retain(|_, v| v.strong_count() != 0);
        servers.insert(base.to_owned(), Arc::downgrade(&trace.0));
        trace
    }

    pub fn for_url(url: &str) -> Option<Self> {
        SERVERS
            .lock()
            .unwrap()
            .iter()
            .find(|(base, _)| url.starts_with(base.as_str()))
            .and_then(|(_, trace)| trace.upgrade())
            .map(Self)
    }

    fn start(&self, method: &str, path: &str) -> u64 {
        let mut trace = self.0.lock().unwrap();
        trace.next += 1;
        let id = trace.next;
        if trace.rows.len() == REQUESTS {
            trace.rows.pop_front();
        }
        trace
            .rows
            .push_back(json!({"id": id, "arrival_ms": now(), "method": method,
            "path": path.chars().take(256).collect::<String>(), "body": "pending"}));
        id
    }

    fn update(&self, id: u64, fields: Value) {
        if let Some(row) = self
            .0
            .lock()
            .unwrap()
            .rows
            .iter_mut()
            .find(|row| row["id"] == id)
        {
            row.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
        }
    }

    pub fn snapshot(&self) -> Value {
        json!(self.0.lock().unwrap().rows)
    }

    pub async fn record(self, request: Request, next: Next) -> Response {
        let id = self.start(request.method().as_str(), request.uri().path());
        let end = BodyEnd {
            trace: self,
            id,
            eof: false,
        };
        let response = next.run(request).await;
        end.wrap(response)
    }
}

struct BodyEnd {
    trace: Requests,
    id: u64,
    eof: bool,
}
impl BodyEnd {
    fn wrap(self, response: Response) -> Response {
        self.trace.update(
            self.id,
            json!({"headers_ms": now(), "status": response.status().as_u16()}),
        );
        let (parts, body) = response.into_parts();
        let stream = futures_util::stream::unfold(
            (body.into_data_stream(), self),
            |(mut stream, mut end)| async move {
                match stream.next().await {
                    Some(item) => {
                        if item.is_err() {
                            end.trace.update(end.id, json!({"body_error_ms": now()}));
                        }
                        Some((item, (stream, end)))
                    }
                    None => {
                        end.eof = true;
                        drop(end);
                        None
                    }
                }
            },
        );
        Response::from_parts(parts, Body::from_stream(stream))
    }
}

impl Drop for BodyEnd {
    fn drop(&mut self) {
        self.trace.update(
            self.id,
            json!({"body_end_ms": now(),
            "body": if self.eof { "eof" } else { "dropped_or_cancelled" }}),
        );
    }
}

/// A single cheap runtime task, no OS sampling on successful calls.
pub struct Heartbeat {
    start: Instant,
    state: Arc<Mutex<(Duration, Duration)>>,
    task: tokio::task::JoinHandle<()>,
}
impl Heartbeat {
    pub fn start() -> Self {
        let start = Instant::now();
        let state = Arc::new(Mutex::new((Duration::ZERO, Duration::ZERO)));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let elapsed = start.elapsed();
                let mut state = shared.lock().unwrap();
                state.1 = state.1.max(elapsed.saturating_sub(state.0));
                state.0 = elapsed;
            }
        });
        Self { start, state, task }
    }
    pub fn snapshot(&self) -> Value {
        let (last, max) = *self.state.lock().unwrap();
        json!({"last_tick_age_ms": self.start.elapsed().saturating_sub(last).as_millis(),
            "max_tick_gap_ms": max.as_millis()})
    }
}
impl Drop for Heartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn descendants(root: u32, parents: &BTreeMap<u32, u32>) -> BTreeSet<u32> {
    let mut found = BTreeSet::from([root]);
    loop {
        let before = found.len();
        for (&pid, parent) in parents {
            if found.contains(parent) {
                found.insert(pid);
            }
        }
        if before == found.len() {
            return found;
        }
    }
}

/// Two OS snapshots on a blocking worker; no shell, PowerShell or extra process.
pub fn os_samples(root: u32) -> Value {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut system = System::new();
    let mut samples = Vec::new();
    for sample in 0..2 {
        if sample != 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        system.refresh_cpu_usage();
        system.refresh_memory();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_cmd(UpdateKind::Always),
        );
        let parents = system
            .processes()
            .iter()
            .map(|(pid, p)| (pid.as_u32(), p.parent().map_or(0, |p| p.as_u32())))
            .collect();
        let pids = descendants(root, &parents);
        let processes: Vec<_> = system.processes().iter().filter(|(pid, _)|
            pids.contains(&pid.as_u32()) || pid.as_u32() == std::process::id())
            .map(|(pid, p)| json!({"pid": pid.as_u32(), "parent": p.parent().map(|p| p.as_u32()),
                "name": p.name().to_string_lossy(), "start_s": p.start_time(),
                "type": p.cmd().iter().find_map(|arg| arg.to_str().filter(|s| s.starts_with("--type="))),
                "status": format!("{:?}", p.status()), "cpu_ms": p.accumulated_cpu_time(),
                "cpu_percent": p.cpu_usage(), "memory_bytes": p.memory()})).collect();
        let load = System::load_average();
        samples.push(json!({"at_ms": now(), "process_count": system.processes().len(),
            "cpu_percent": system.global_cpu_usage(), "available_memory": system.available_memory(),
            "total_memory": system.total_memory(), "load_average": [load.one, load.five, load.fifteen],
            "processes": processes}));
    }
    json!(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn body_log_distinguishes_headers_eof_and_open_streams() {
        let trace = Requests(Arc::default());
        let id = trace.start("GET", "/");
        let response = BodyEnd {
            trace: trace.clone(),
            id,
            eof: false,
        }
        .wrap(Response::new(Body::from("hello")));
        assert_eq!(trace.snapshot()[0]["status"], 200);
        assert_eq!(trace.snapshot()[0]["body"], "pending");
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), b"hello");
        assert_eq!(trace.snapshot()[0]["body"], "eof");
        let id = trace.start("GET", "/events");
        let body = Body::from_stream(futures_util::stream::pending::<
            Result<String, std::io::Error>,
        >());
        let response = BodyEnd {
            trace: trace.clone(),
            id,
            eof: false,
        }
        .wrap(Response::new(body));
        assert_eq!(trace.snapshot()[1]["body"], "pending");
        drop(response);
        assert_eq!(trace.snapshot()[1]["body"], "dropped_or_cancelled");
    }

    #[test]
    fn request_history_is_bounded_and_keeps_inflight_and_body_states() {
        let trace = Requests(Arc::default());
        let old = trace.start("GET", "/old");
        for _ in 0..REQUESTS {
            trace.start("GET", "/new");
        }
        trace.update(old, json!({"status": 500}));
        let id = trace.0.lock().unwrap().next;
        trace.update(id, json!({"status": 200, "headers_ms": now()}));
        let snapshot = trace.snapshot();
        assert_eq!(snapshot.as_array().unwrap().len(), REQUESTS);
        assert_eq!(snapshot[0]["path"], "/new");
        assert_eq!(snapshot[REQUESTS - 1]["body"], "pending");
        drop(BodyEnd {
            trace: trace.clone(),
            id,
            eof: true,
        });
        assert_eq!(trace.snapshot()[REQUESTS - 1]["body"], "eof");
    }
    #[test]
    fn process_tree_excludes_siblings_and_tolerates_cycles() {
        let parents = BTreeMap::from([(1, 0), (2, 1), (3, 2), (4, 1), (5, 6), (6, 5)]);
        assert_eq!(descendants(2, &parents), BTreeSet::from([2, 3]));
        assert_eq!(descendants(5, &parents), BTreeSet::from([5, 6]));
    }
}
