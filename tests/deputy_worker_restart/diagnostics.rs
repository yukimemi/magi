//! Failure-only, read-only evidence, captured before any owned resource drops.
use super::{Release, Scheduler, common};
use magi::ask::{Question, Questions};
use magi::worker::Job;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

pub(super) struct Scenario {
    // Fields drop in order, after the diagnostic Drop body.
    pub schedulers: Vec<Scheduler>,
    pub release: Option<Release>,
    pub fx: common::Fixture,
    pub questions: Vec<String>,
    pub original: Option<Job>,
}

impl Scenario {
    pub fn new(fx: common::Fixture) -> Self {
        Self {
            schedulers: Vec::new(),
            release: None,
            fx,
            questions: Vec::new(),
            original: None,
        }
    }

    fn dump(&mut self) {
        let home = magi::run::home();
        let mut out = Report::default();
        out.line(format!(
            "deputy restart failure diagnostics at {:?}; sequential observations, not an atomic snapshot",
            std::time::SystemTime::now()
        ));
        out.line("A missing active identity is not proof of process exit; PID is diagnostic only.");
        for (index, scheduler) in self.schedulers.iter_mut().enumerate() {
            out.line(format!(
                "scheduler[{index}] pid={} process observation={:?}",
                scheduler.0.id(),
                scheduler.0.try_wait()
            ));
        }
        match &self.original {
            Some(job) => out.job("original worker", job),
            None => out.line("original worker: not yet captured"),
        }
        match read_json::<magi::daemon::Reading>(&home.join("daemon.json")) {
            Ok(status) => out.line(format!(
                "daemon pid={:?} polls={} updated_at={:?}",
                status.pid, status.polls, status.updated_at
            )),
            Err(error) => out.line(format!("daemon: {error}")),
        }
        match magi::worker::observe_active(&home) {
            Ok(jobs) => {
                out.line(format!(
                    "observe_active total={} deputies={} original_present={:?}",
                    jobs.len(),
                    jobs.iter().filter(|j| j.kind == "deputy").count(),
                    self.original
                        .as_ref()
                        .map(|old| jobs.iter().any(|j| j.id == old.id && j.pid == old.pid))
                ));
                for job in jobs.iter().take(16) {
                    out.job("active", job);
                }
                if jobs.len() > 16 {
                    out.line("active identities omitted after 16");
                }
            }
            Err(error) => out.line(format!("observe_active: unknown/unreadable: {error:#}")),
        }
        let store = Questions::at(home.join("questions"));
        let state = self.fx.tmp.path().join("deputy-state");
        out.marker("common release", &state.join("release"));
        for (index, label) in ["first", "second", "third"].iter().enumerate() {
            let Some(id) = self.questions.get(index) else {
                out.line(format!("{label} Question: not yet registered"));
                continue;
            };
            out.line(format!("{label} Question id={id}"));
            match read_json::<Question>(&store.path_of(id)) {
                Ok(q) => {
                    out.line(format!(
                        "status={:?} answer_present={} answered_at={:?}",
                        q.status,
                        q.answer.is_some(),
                        q.answered_at
                    ));
                    if let Some(d) = q.deputy {
                        out.line(format!(
                            "deputy starts={} agent={} seat={:?}",
                            d.starts,
                            d.agent,
                            d.seat.map(|s| (s.key, s.agent, s.turns))
                        ));
                    } else {
                        out.line("deputy: absent");
                    }
                }
                Err(error) => out.line(format!("Question: {error}")),
            }
            match read_json::<magi::ask::Lease>(&store.lease_path(id)) {
                Ok(lease) => out.line(format!("lease: {lease:?}")),
                Err(error) => out.line(format!("lease: {error}")),
            }
            let seat = magi::ask::deputy_seat_key(id);
            for suffix in ["started", "release"] {
                out.marker(
                    &format!("{label} {suffix}"),
                    &state.join(format!("{seat}.{suffix}")),
                );
            }
            let artifacts = home.join("questions").join(format!("{id}.deputy"));
            let mut outputs: Vec<_> = entries(&artifacts, &mut out)
                .into_iter()
                .filter(|p| matches!(p.extension().and_then(|s| s.to_str()), Some("out" | "err")))
                .collect();
            outputs.sort();
            if outputs.is_empty() {
                out.line(format!("{label} deputy output/error: no files observed"));
            }
            if outputs.len() > 4 {
                out.line(format!("{label} output/error files omitted after 4"));
            }
            for path in outputs.into_iter().take(4) {
                out.log(
                    &format!(
                        "{label} deputy output/error {}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ),
                    &path,
                );
            }
        }
        for name in ["old.log", "new.log"] {
            out.log(name, &self.fx.tmp.path().join(name));
        }
        for state in ["jobs", "results"] {
            let directory = home.join("workers").join(state);
            let mut paths = entries(&directory, &mut out);
            // Keep the known original identity even if enumeration was capped.
            if let Some(original) = &self.original {
                let path = directory.join(&original.id);
                if !paths.contains(&path) {
                    paths.insert(0, path);
                }
            }
            let mut records: Vec<_> = paths
                .into_iter()
                .map(|path| {
                    let job = read_json::<Job>(&path.join("job.json"));
                    let relevant = job.as_ref().is_ok_and(|j| {
                        self.original.as_ref().is_some_and(|old| old.id == j.id)
                            || self
                                .questions
                                .iter()
                                .any(|id| j.key == format!("deputy:{id}"))
                    });
                    (relevant, path, job)
                })
                .collect();
            records.sort_by_key(|(relevant, _, _)| !relevant);
            if records.len() > 16 {
                out.line(format!(
                    "workers/{state}: records omitted after 16 (related records first)"
                ));
            }
            for (_, path, job) in records.into_iter().take(16) {
                let label = format!(
                    "workers/{state}/{}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                );
                match job {
                    Ok(job) => out.job(&label, &job),
                    Err(error) => out.line(format!("{label} job.json: {error}")),
                }
                out.marker(
                    &format!("{label} live.lock (existence only, not lock ownership)"),
                    &path.join("live.lock"),
                );
                out.log(
                    &format!("{label}/worker.log (worker stdout/stderr)"),
                    &path.join("worker.log"),
                );
            }
        }
        let _ = std::io::stderr().lock().write_all(out.text.as_bytes());
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Diagnostics must never replace the assertion (including a diagnostic panic).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dump()));
        }
    }
}

// Do not print payloads, prompts, answer bodies, sessions, or full configuration.
#[derive(Default)]
struct Report {
    text: String,
}
impl Report {
    fn line(&mut self, value: impl AsRef<str>) {
        const TOTAL: usize = 256 * 1024;
        if self.text.len() >= TOTAL {
            return;
        }
        let value = value.as_ref();
        let limit = (TOTAL - self.text.len()).min(4608);
        let mut end = value.len().min(limit);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        self.text.push_str(&value[..end]);
        if end < value.len() {
            self.text.push_str(" [truncated]");
        }
        self.text.push('\n');
        if self.text.len() >= TOTAL {
            self.text
                .push_str("[diagnostic output limit reached; remainder omitted]\n");
        }
    }
    fn job(&mut self, label: &str, j: &Job) {
        self.line(format!("{label}: id={} key={} kind={} pid={:?} generation={} status={:?} sequence={} created={} finished={:?} result={:?}", j.id, j.key, j.kind, j.pid, j.generation, j.status, j.sequence, j.created_at, j.finished_at, j.result.as_ref().map(|s| magi::run::tail(s, 1024))));
    }
    fn marker(&mut self, label: &str, path: &Path) {
        self.line(match std::fs::metadata(path) {
            Ok(m) => format!(
                "{label}: present bytes={} modified={:?}",
                m.len(),
                m.modified()
            ),
            Err(e) => format!("{label}: {}", io_error(e)),
        });
    }
    fn log(&mut self, label: &str, path: &Path) {
        // Same seek + take convention as web_render/cdp::file_tail.
        let read = || -> std::io::Result<String> {
            let mut file = std::fs::File::open(path)?;
            let len = file.metadata()?.len();
            file.seek(SeekFrom::Start(len.saturating_sub(4096)))?;
            let mut bytes = Vec::new();
            file.take(4096).read_to_end(&mut bytes)?;
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<_> = text.lines().collect();
            Ok(format!(
                "bytes={len}; tail <=4096 bytes/40 lines (earlier content omitted if longer):\n{}",
                magi::run::tail(&lines[lines.len().saturating_sub(40)..].join("\n"), 4096)
            ))
        };
        self.line(format!("{label}: {}", read().unwrap_or_else(io_error)));
    }
}
fn io_error(e: std::io::Error) -> String {
    format!(
        "{}: {e}",
        if e.kind() == std::io::ErrorKind::NotFound {
            "missing"
        } else {
            "unreadable"
        }
    )
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let file = std::fs::File::open(path).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.take(256 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() > 256 * 1024 {
        return Err("unknown: record exceeds 256 KiB read limit".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("unparseable: {e}"))
}

fn entries(path: &Path, out: &mut Report) -> Vec<std::path::PathBuf> {
    match std::fs::read_dir(path) {
        Ok(mut entries) => {
            let paths = entries
                .by_ref()
                .take(64)
                .filter_map(|entry| match entry {
                    Ok(e) => Some(e.path()),
                    Err(e) => {
                        out.line(format!("directory entry: {}", io_error(e)));
                        None
                    }
                })
                .collect();
            if entries.next().is_some() {
                out.line("directory entries omitted after 64");
            }
            paths
        }
        Err(e) => {
            out.line(format!(
                "directory {}: {}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                io_error(e)
            ));
            Vec::new()
        }
    }
}
