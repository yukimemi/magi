//! Diagnostic observations never authorize deletion. The frozen run list and
//! per-seat continuation are independent of mutable run bookkeeping timestamps.
use super::*;

const MAX_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct CheckpointFailure;
impl std::fmt::Display for CheckpointFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("persist source scan census")
    }
}
impl std::error::Error for CheckpointFailure {}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Observation {
    pub outcome: String,
    pub stage: String,
    pub detail: String,
    pub at: Timestamp,
}

impl Observation {
    pub fn new(outcome: &str, stage: &str, detail: impl Into<String>, at: Timestamp) -> Self {
        Self {
            outcome: outcome.into(),
            stage: stage.into(),
            detail: detail.into(),
            at,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Visit {
    root_path: PathBuf,
    root: Observation,
    seats: BTreeMap<String, Observation>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cycle {
    number: u64,
    started_at: Timestamp,
    completed_at: Option<Timestamp>,
    upper_id: Option<String>,
    runs: BTreeMap<String, Visit>,
    next: usize,
    after_seat: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Scan {
    version: u32,
    current: Cycle,
    previous: Option<Cycle>,
}

impl Cycle {
    fn new(number: u64, states: &[RunState], now: Timestamp) -> Self {
        Self {
            number,
            started_at: now,
            completed_at: None,
            upper_id: states.last().map(|s| s.id.clone()),
            runs: states
                .iter()
                .map(|s| {
                    (
                        s.id.clone(),
                        Visit {
                            root_path: s.worktree_root(),
                            root: Observation::new(
                                "not-yet-inspected",
                                "discovery",
                                "run root discovery pending",
                                now,
                            ),
                            seats: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
            next: 0,
            after_seat: None,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.number > 0 && (self.completed_at.is_none() || self.next == self.runs.len()),
            "invalid scan completion"
        );
        ensure!(
            self.after_seat.is_none() || self.next < self.runs.len(),
            "invalid completed seat continuation"
        );
        ensure!(
            self.runs.len() <= 4096 && self.next <= self.runs.len(),
            "invalid scan position"
        );
        ensure!(
            self.runs.keys().all(|id| crate::run::is_run_id(id)),
            "invalid scan run identity"
        );
        ensure!(
            self.upper_id.as_ref() == self.runs.keys().next_back(),
            "invalid scan upper bound"
        );
        ensure!(
            self.after_seat
                .as_ref()
                .is_none_or(|name| safe_component(name)),
            "invalid seat continuation"
        );
        if let Some(after) = &self.after_seat {
            ensure!(
                self.runs
                    .values()
                    .nth(self.next)
                    .and_then(|visit| visit.seats.get(after))
                    .is_some_and(|seat| matches!(
                        seat.outcome.as_str(),
                        "evicted" | "protected" | "error"
                    )),
                "seat continuation lacks a durable inspection outcome"
            );
        }
        for visit in self.runs.values() {
            for observation in std::iter::once(&visit.root).chain(visit.seats.values()) {
                ensure!(
                    matches!(
                        observation.outcome.as_str(),
                        "not-yet-inspected" | "inspected" | "protected" | "error" | "evicted"
                    ),
                    "unknown scan outcome"
                );
            }
            ensure!(
                visit.seats.len() <= 4096 && visit.seats.keys().all(|name| safe_component(name)),
                "invalid scan seat inventory"
            );
        }
        Ok(())
    }
}

fn safe_component(name: &str) -> bool {
    !name.is_empty()
        && Path::new(name).file_name().and_then(|s| s.to_str()) == Some(name)
        && name != "."
        && name != ".."
}

impl Scan {
    pub fn load(home: &Path, states: &[RunState], now: Timestamp) -> Result<Self> {
        let path = home.join("source-eviction-cursor");
        let mut scan = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self {
                version: 1,
                current: Cycle::new(1, states, now),
                previous: None,
            },
            Err(e) => return Err(e.into()),
            Ok(meta) => {
                ensure!(
                    meta.is_file()
                        && !meta.file_type().is_symlink()
                        && meta.len() <= MAX_BYTES
                        && path.canonicalize()? == path,
                    "unknown scan cursor retained"
                );
                let bytes = std::fs::read(&path)?;
                if let Ok(last) = std::str::from_utf8(&bytes)
                    && crate::run::is_run_id(last)
                {
                    // A legacy timestamp-ordered cursor proves no completed census.
                    // Start a new stable cycle; never inherit its skipped seats.
                    Self {
                        version: 1,
                        current: Cycle::new(1, states, now),
                        previous: None,
                    }
                } else {
                    serde_json::from_slice(&bytes).context("invalid scan cursor retained")?
                }
            }
        };
        ensure!(scan.version == 1, "unknown scan cursor version");
        scan.current.validate()?;
        if let Some(previous) = &scan.previous {
            previous.validate()?;
        }
        if scan.current.completed_at.is_some() {
            let number = scan
                .current
                .number
                .checked_add(1)
                .context("scan cycle overflow")?;
            scan.previous = Some(std::mem::replace(
                &mut scan.current,
                Cycle::new(number, states, now),
            ));
        }
        Ok(scan)
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        (|| {
            let bytes = serde_json::to_vec(self)?;
            ensure!(
                bytes.len() as u64 <= MAX_BYTES,
                "scan census storage budget exceeded"
            );
            crate::fold_evidence::atomic_write(&home.join("source-eviction-cursor"), &bytes)
        })()
        .context(CheckpointFailure)
    }

    pub fn next(&self) -> Option<String> {
        self.current.runs.keys().nth(self.current.next).cloned()
    }
    pub fn after(&self) -> Option<&str> {
        self.current.after_seat.as_deref()
    }
    pub fn validate_root(&self, id: &str, root: &Path) -> Result<()> {
        ensure!(
            self.current.runs.get(id).context("scan run")?.root_path == root,
            "source root changed since census began; deferred until next cycle"
        );
        Ok(())
    }
    pub fn root(&mut self, id: &str, observation: Observation) {
        self.current.runs.get_mut(id).expect("frozen run").root = observation;
    }
    pub fn discover(&mut self, id: &str, names: impl Iterator<Item = String>, now: Timestamp) {
        let visit = self.current.runs.get_mut(id).expect("frozen run");
        for name in names {
            visit.seats.entry(name).or_insert_with(|| {
                Observation::new(
                    "not-yet-inspected",
                    "seat-budget",
                    "awaiting bounded seat inspection",
                    now,
                )
            });
        }
    }
    pub fn seat(&mut self, id: &str, name: &str, observation: Observation) {
        self.current
            .runs
            .get_mut(id)
            .expect("frozen run")
            .seats
            .insert(name.into(), observation);
        self.current.after_seat = Some(name.into());
    }
    pub fn registration_gaps(&mut self, id: &str, paths: &[PathBuf], now: Timestamp) {
        let visit = self.current.runs.get_mut(id).expect("frozen run");
        for (name, observation) in &mut visit.seats {
            if observation.outcome == "not-yet-inspected"
                && !paths
                    .iter()
                    .any(|p| p.file_name().and_then(|s| s.to_str()) == Some(name.as_str()))
            {
                *observation = Observation::new(
                    "protected",
                    "registration",
                    "discovered source is absent from canonical registered worktrees; no removal authorized",
                    now,
                );
            }
        }
    }
    pub fn finish_run(&mut self, id: &str, blocker: Option<&Observation>, now: Timestamp) {
        let visit = self.current.runs.get_mut(id).expect("frozen run");
        if let Some(blocker) = blocker {
            visit.root = blocker.clone();
            for seat in visit
                .seats
                .values_mut()
                .filter(|s| s.outcome == "not-yet-inspected")
            {
                *seat = blocker.clone();
            }
        }
        self.current.next += 1;
        self.current.after_seat = None;
        if self.current.next == self.current.runs.len() {
            self.current.completed_at = Some(now);
        }
    }
    pub fn complete_empty(&mut self, now: Timestamp) {
        if self.current.runs.is_empty() {
            self.current.completed_at = Some(now);
        }
    }
    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({"cycle": self.current.number, "started_at": self.current.started_at,
            "completed_at": self.current.completed_at, "upper_id": self.current.upper_id,
            "runs": self.current.runs.len(), "runs_finished": self.current.next,
            "next_run": self.next(), "after_seat": self.after(),
            "previous_completed_cycle": self.previous.as_ref().map(|c| c.number)})
    }
}

pub(super) fn shown(home: &Path, id: &str) -> Result<Vec<String>> {
    let path = home.join("source-eviction-cursor");
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let meta = std::fs::symlink_metadata(&path)?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.len() <= MAX_BYTES
            && path.canonicalize()? == path,
        "unknown scan cursor retained"
    );
    let bytes = std::fs::read(&path)?;
    if std::str::from_utf8(&bytes).is_ok_and(crate::run::is_run_id) {
        return Ok(Vec::new());
    }
    let scan: Scan = serde_json::from_slice(&bytes)?;
    ensure!(scan.version == 1, "unknown scan cursor version");
    scan.current.validate()?;
    let mut lines = Vec::new();
    for cycle in std::iter::once(&scan.current).chain(scan.previous.iter()) {
        if let Some(visit) = cycle.runs.get(id) {
            for (name, observation) in std::iter::once(("root", &visit.root))
                .chain(visit.seats.iter().map(|(n, o)| (n.as_str(), o)))
            {
                lines.push(format!(
                    "scan cycle={} complete={} {} {} stage={} at={} reason={}",
                    cycle.number,
                    cycle.completed_at.is_some(),
                    name,
                    observation.outcome,
                    observation.stage,
                    observation.at,
                    observation.detail
                ));
            }
        }
    }
    Ok(lines)
}
