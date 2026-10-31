//! Durable task-owned free-text deliveries, independent of decision execution.
//!
//! A delivery receipt means only that the conversation accepted a report. It
//! never records an owner request or authorizes a task, publication or merge.
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::{
    queue::Queue,
    talk::{self, Talks},
};

/// Free-text input owed to a task's conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Inquiry {
    /// Durable format version.
    pub schema: u32,
    /// Stable producer-assigned delivery identity.
    pub id: String,
    /// Exact owning task; prefixes are never resolved on delivery.
    pub task: String,
    /// Execution the inquiry concerns, if any.
    pub run: Option<String>,
    /// Repository captured when the inquiry was filed.
    pub repo: PathBuf,
    /// Producing graph node or service.
    pub source: String,
    /// Complete free-text agent input, with no executable choices.
    pub text: String,
    /// User-visible report facts.
    pub display: String,
    /// Source delivery key, retained across a transport migration.
    pub key: String,
    /// Existing automatic-notice evidence identity.
    pub evidence: Option<String>,
    /// When the inquiry was filed.
    pub created_at: Timestamp,
    /// Conversation that durably accepted it. This is not an action receipt.
    pub delivered_to: Option<String>,
}

/// File-backed outbox under the existing notifications write grant.
#[derive(Debug, Clone)]
pub struct Outbox {
    root: PathBuf,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn same_repository(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

impl Outbox {
    /// Open an outbox beneath an explicit state home.
    pub fn at(home: &Path) -> Self {
        Self {
            root: home.join("notifications").join("task-inquiries"),
        }
    }

    fn path(&self, id: &str) -> Result<PathBuf> {
        ensure!(valid_id(id), "invalid inquiry id");
        Ok(self.root.join(format!("{id}.json")))
    }

    fn lock(&self, id: &str) -> Result<File> {
        self.path(id)?;
        std::fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(format!("{id}.lock")))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match crate::pr_head::try_lock_file(&file) {
                Ok(()) => return Ok(file),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(33) =>
                {
                    ensure!(Instant::now() < deadline, "inquiry {id} is busy");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn save(&self, inquiry: &Inquiry) -> Result<()> {
        let path = self.path(&inquiry.id)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(inquiry)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }

    /// Read one delivery without consulting legacy question records.
    pub fn get(&self, id: &str) -> Result<Inquiry> {
        let inquiry: Inquiry = serde_json::from_slice(&std::fs::read(self.path(id)?)?)?;
        ensure!(
            inquiry.schema == 1 && inquiry.id == id,
            "unsupported or mismatched inquiry record"
        );
        Ok(inquiry)
    }

    /// Persist before delivery. Retries retain the original input and receipt.
    pub fn enqueue(&self, inquiry: Inquiry) -> Result<Inquiry> {
        let _lock = self.lock(&inquiry.id)?;
        if self.path(&inquiry.id)?.exists() {
            let saved = self.get(&inquiry.id)?;
            ensure!(
                saved.task == inquiry.task
                    && saved.run == inquiry.run
                    && saved.repo == inquiry.repo
                    && saved.source == inquiry.source
                    && saved.key == inquiry.key,
                "inquiry identity was reused for a different target"
            );
            return Ok(saved);
        }
        ensure!(
            inquiry.schema == 1 && inquiry.delivered_to.is_none(),
            "new inquiries cannot carry delivery receipts"
        );
        ensure!(!inquiry.text.trim().is_empty(), "inquiry text is empty");
        self.save(&inquiry)?;
        Ok(inquiry)
    }

    /// Deliver under a per-inquiry lease, releasing the task lock before any
    /// conversation operation. The transcript key repairs a lost final receipt.
    pub fn deliver(&self, queue: &Queue, talks: &Talks, id: &str) -> Result<String> {
        let _lock = self.lock(id)?;
        let mut inquiry = self.get(id)?;
        let (_, mut chat) =
            crate::task_chat::destination_checked(queue, talks, &inquiry.task, |task| {
                ensure!(
                    same_repository(&task.repo, &inquiry.repo),
                    "inquiry repository no longer matches its task"
                );
                ensure!(
                    inquiry
                        .run
                        .as_ref()
                        .is_none_or(|run| task.runs.contains(run)),
                    "inquiry run is not owned by its task"
                );
                Ok(())
            })?;
        if let Some(delivered) = &inquiry.delivered_to {
            ensure!(
                delivered == &chat.id,
                "delivered inquiry changed conversations"
            );
            return Ok(delivered.clone());
        }
        talk::queue_semantic_notice(
            &mut chat,
            talks,
            &inquiry.text,
            &inquiry.key,
            &|_| false,
            (
                None,
                Some(
                    inquiry
                        .evidence
                        .clone()
                        .unwrap_or_else(|| format!("task-inquiry:{}", inquiry.id)),
                ),
            ),
            crate::prompt::NoticeDisplay {
                facts: inquiry.display.clone(),
                output: None,
                task: Some(inquiry.task.clone()),
                run: inquiry.run.clone(),
            },
        )?;
        inquiry.delivered_to = Some(chat.id.clone());
        self.save(&inquiry)?;
        Ok(chat.id)
    }

    fn records(&self) -> Result<Vec<Result<Inquiry>>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths
            .into_iter()
            .map(|path| {
                let id = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .context("invalid inquiry filename")?;
                self.get(id).with_context(|| format!("read inquiry {id}"))
            })
            .collect())
    }

    /// Read pending deliveries. Corrupt records are errors, never silently skipped.
    pub fn pending(&self) -> Result<Vec<Inquiry>> {
        Ok(self
            .records()?
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|inquiry| inquiry.delivered_to.is_none())
            .collect())
    }

    /// Resume durable deliveries after a scheduler restart without starting agents.
    pub fn sweep(&self, queue: &Queue, talks: &Talks) -> Result<()> {
        let mut failed = None;
        for record in self.records()? {
            let inquiry = match record {
                Ok(inquiry) => inquiry,
                Err(error) => {
                    tracing::warn!("task inquiry record is unreadable: {error:#}");
                    failed.get_or_insert(error);
                    continue;
                }
            };
            if inquiry.delivered_to.is_some() {
                continue;
            }
            if let Err(error) = self.deliver(queue, talks, &inquiry.id) {
                tracing::warn!("inquiry {} delivery failed: {error:#}", inquiry.id);
                failed.get_or_insert(error);
            }
        }
        failed.map_or(Ok(()), Err)
    }
}
