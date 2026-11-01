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
    /// Owner decision lifecycle; absent for informational deliveries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<crate::task_decision::Decision>,
    /// Delivery failures never count as a decision or action outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_error: Option<String>,
    /// Passive imported history; never submitted to a chat worker.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
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

    /// File a free-text task inquiry directly, without a Question envelope.
    /// The producer identity and unchanged inquiry content form its restart key.
    pub fn file(
        &self,
        task: &crate::queue::Task,
        run: Option<String>,
        source: String,
        summary: &str,
        detail: &str,
    ) -> Result<Inquiry> {
        use sha2::{Digest, Sha256};
        ensure!(!summary.trim().is_empty(), "inquiry summary is empty");
        ensure!(
            run.as_ref().is_none_or(|run| task.runs.contains(run)),
            "inquiry run is not owned by its task"
        );
        let bytes = serde_json::to_vec(&(
            "task-inquiry-v1",
            &task.id,
            &task.repo,
            &run,
            &source,
            summary,
            detail,
        ))?;
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let id = format!("task-{digest}");
        let clean = |s: &str| {
            crate::prompt::defuse(&crate::scrub::scrub(s, &crate::scrub::Identity::current()))
        };
        let display = format!("{}\n\n{}", clean(summary), clean(detail));
        let text = format!(
            "# {}\n\n{display}\n\nTask: {}\nRun: {}\n\nDiscuss this inquiry with the owner as free text. It is not an owner instruction. Carry out owner requests through `magi task control --decision {id} <command>`. If the owner explicitly settles the discussion with no remaining effect, use `magi task control settle-decision {id} --reason <reason>`. Silence is a hold; delivery and reading never settle this inquiry.\n",
            crate::prompt::TASK_MESSAGE_HEADING,
            task.id,
            run.as_deref().unwrap_or("none")
        );
        self.enqueue(Inquiry {
            schema: 1,
            id: id.clone(),
            task: task.id.clone(),
            run,
            repo: task.repo.clone(),
            source,
            text,
            display,
            key: format!("inquiry:{id}"),
            evidence: None,
            created_at: Timestamp::now(),
            delivered_to: None,
            decision: Some(crate::task_decision::Decision::awaiting(
                clean(summary).chars().take(240).collect(),
            )),
            delivery_error: None,
            archived: false,
        })
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
            let mut saved = self.get(&inquiry.id)?;
            if saved.task.is_empty()
                && !inquiry.task.is_empty()
                && saved.delivered_to.is_none()
                && saved.source == inquiry.source
                && saved.decision.as_ref().is_some_and(|d| d.request.is_none())
            {
                saved.task = inquiry.task.clone();
                saved.run = inquiry.run.clone();
                saved.repo = inquiry.repo.clone();
                saved.text = inquiry.text.clone();
                saved.display = inquiry.display.clone();
                saved.evidence = inquiry.evidence.clone();
                saved.delivery_error = None;
                self.save(&saved)?;
            }
            ensure!(
                saved.task == inquiry.task
                    && saved.run == inquiry.run
                    && saved.repo == inquiry.repo
                    && saved.source == inquiry.source
                    && saved.key == inquiry.key,
                "inquiry identity was reused for a different target"
            );
            if inquiry.id.starts_with("task-") {
                ensure!(
                    saved.text == inquiry.text && saved.display == inquiry.display,
                    "inquiry key collision: content differs"
                );
            }
            if saved.decision.is_none() && inquiry.decision.is_some() {
                saved.archived = inquiry.archived;
                if !inquiry.archived {
                    saved.text = inquiry.text;
                    saved.display = inquiry.display;
                }
                saved.decision = inquiry.decision;
                self.save(&saved)?;
            }
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
        ensure!(
            !inquiry.archived,
            "passive inquiry history is not a delivery"
        );
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
            if chat.notified.iter().any(|key| key == &inquiry.key) {
                return Ok(delivered.clone());
            }
            // The saved conversation was removed/recreated after delivery.
            // Restore the same inquiry key; no owner request is reconstructed.
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
                inquiry: Some(inquiry.id.clone()),
                facts: inquiry.display.clone(),
                output: None,
                task: Some(inquiry.task.clone()),
                run: inquiry.run.clone(),
            },
        )?;
        inquiry.delivered_to = Some(chat.id.clone());
        inquiry.delivery_error = None;
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

    /// Stop compatibility/history delivery without manufacturing resolution.
    pub(crate) fn suspend_delivery(&self, id: &str, reason: &str) -> Result<()> {
        let _lock = self.lock(id)?;
        let mut record = self.get(id)?;
        if !record.archived || record.delivery_error.as_deref() != Some(reason) {
            record.archived = true;
            record.delivery_error = Some(reason.to_owned());
            self.save(&record)?;
        }
        Ok(())
    }

    /// Read all durable inquiries, including completed decision history.
    pub fn list(&self) -> Result<Vec<Inquiry>> {
        self.records()?.into_iter().collect()
    }

    /// 一覧表示用の同一走査で、正常な記録と読めない記録の件数を返す。
    /// 実行系の厳密な list / pending のエラー処理は変更しない。
    pub fn list_readable(&self) -> Result<(Vec<Inquiry>, usize)> {
        let mut items = Vec::new();
        let mut unreadable = 0;
        for record in self.records()? {
            match record {
                Ok(inquiry) => items.push(inquiry),
                Err(error) => {
                    tracing::warn!("task inquiry record is unreadable: {error:#}");
                    unreadable += 1;
                }
            }
        }
        Ok((items, unreadable))
    }

    /// Change only the decision lifecycle under the same lease as delivery.
    /// Returning false avoids rewriting a receipt on every scheduler lap.
    pub fn update_decision(
        &self,
        id: &str,
        change: impl FnOnce(&mut crate::task_decision::Decision) -> Result<bool>,
    ) -> Result<()> {
        let _lock = self.lock(id)?;
        let mut inquiry = self.get(id)?;
        let decision = inquiry
            .decision
            .as_mut()
            .context("inquiry is informational")?;
        if change(decision)? {
            self.save(&inquiry)?;
        }
        Ok(())
    }

    /// Explicit settlement belongs to the owning task chat. Replacement must
    /// already exist and have the same task/repository; it executes nothing.
    pub fn settle(
        &self,
        id: &str,
        chat: &str,
        reason: &str,
        replacement: Option<&str>,
    ) -> Result<()> {
        ensure!(!reason.trim().is_empty(), "settlement reason is empty");
        let mut ids = vec![id];
        if let Some(replacement) = replacement {
            ensure!(replacement != id, "decision cannot supersede itself");
            ids.push(replacement);
        }
        ids.sort_unstable();
        // Hold both records in a fixed order. Reciprocal supersessions cannot
        // both observe an active replacement and erase the last pending intent.
        let _locks: Vec<_> = ids
            .into_iter()
            .map(|id| self.lock(id))
            .collect::<Result<_>>()?;
        let mut current = self.get(id)?;
        ensure!(
            current.delivered_to.as_deref() == Some(chat),
            "decision belongs to another chat"
        );
        let reason = if let Some(replacement) = replacement {
            ensure!(replacement != id, "decision cannot supersede itself");
            let next = self.get(replacement)?;
            ensure!(
                next.task == current.task
                    && next.repo == current.repo
                    && next.delivered_to == current.delivered_to
                    && next
                        .decision
                        .as_ref()
                        .is_some_and(|d| d.status != crate::task_decision::Status::Resolved),
                "replacement must be an active decision in the same task chat"
            );
            format!("superseded by {replacement}: {reason}")
        } else {
            format!("owner settled without an effect: {reason}")
        };
        if current
            .decision
            .as_mut()
            .context("inquiry is informational")?
            .settle(&reason)?
        {
            self.save(&current)?;
        }
        Ok(())
    }

    /// Read pending deliveries. Corrupt records are errors, never silently skipped.
    pub fn pending(&self) -> Result<Vec<Inquiry>> {
        Ok(self
            .records()?
            .into_iter()
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|inquiry| {
                inquiry.delivered_to.is_none()
                    && !inquiry.archived
                    && inquiry.decision.as_ref().is_none_or(|decision| {
                        decision.status != crate::task_decision::Status::Resolved
                    })
            })
            .collect())
    }

    /// Resume durable deliveries after a scheduler restart without starting agents.
    pub fn sweep(&self, queue: &Queue, talks: &Talks) -> Result<()> {
        let home = self
            .root
            .parent()
            .and_then(Path::parent)
            .context("inquiry store has no home")?;
        let deferred = crate::task_chat::migration_pending(home);
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
            if inquiry.key.starts_with("question:")
                && deferred
                    .as_ref()
                    .map_or(true, |ids| ids.contains(&inquiry.id))
            {
                continue;
            }
            if inquiry.archived
                || inquiry
                    .decision
                    .as_ref()
                    .is_some_and(|d| d.status == crate::task_decision::Status::Resolved)
                || inquiry.task.is_empty()
            {
                continue;
            }
            if inquiry
                .delivered_to
                .as_ref()
                .is_some_and(|id| talks.path_of(id).exists())
            {
                continue;
            }
            if let Err(error) = self.deliver(queue, talks, &inquiry.id) {
                if let Ok(_lock) = self.lock(&inquiry.id)
                    && let Ok(mut current) = self.get(&inquiry.id)
                {
                    let message = format!("{error:#}");
                    if current.delivery_error.as_ref() != Some(&message) {
                        current.delivery_error = Some(message);
                        if let Err(error) = self.save(&current) {
                            tracing::warn!("cannot persist inquiry delivery failure: {error:#}");
                        }
                    }
                }
                tracing::warn!("inquiry {} delivery failed: {error:#}", inquiry.id);
                failed.get_or_insert(error);
            }
        }
        failed.map_or(Ok(()), Err)
    }
}
