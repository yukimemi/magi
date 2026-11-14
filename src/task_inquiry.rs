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
    /// Producer occurrence generation. Older saved inquiries may not have one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<String>,
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

/// Why a passive compatibility import no longer needs a decision.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryResolution {
    Applied,
    Retired,
}

/// File-backed outbox under the existing notifications write grant.
#[derive(Debug, Clone)]
pub struct Outbox {
    root: PathBuf,
}

/// Release the execution fence even if a concurrent fork still retains a
/// descriptor for the same open file description.
pub(crate) struct ExecutionClaim {
    // Closing the retained file releases the lock on platforms without flock.
    _file: File,
}

#[cfg(unix)]
impl Drop for ExecutionClaim {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: this guard owns the live descriptor; LOCK_UN releases only
        // this open file description's flock.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Stable identity for one producer-assigned occurrence.
fn id_for(task: &str, kind: &str, generation: &str) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(&("task-inquiry-v3", task, kind, generation))
        .expect("string tuple serialization cannot fail");
    let digest: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("task-v3-{digest}")
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

    /// Shared with legacy consumers: acquire before reading an executable reply
    /// and retain through its effect and receipt. Nonblocking so a settlement
    /// cannot wait for (and then hide) an operation currently in flight.
    pub(crate) fn execution_claim(&self, id: &str) -> Result<ExecutionClaim> {
        self.path(id)?;
        std::fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join(format!("{id}.execution.lock")))?;
        crate::pr_head::try_lock_file(&file)
            .with_context(|| format!("effect consumer for inquiry {id} is busy"))?;
        Ok(ExecutionClaim { _file: file })
    }

    fn save(&self, inquiry: &Inquiry) -> Result<()> {
        let path = self.path(&inquiry.id)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(inquiry)?)?;
        std::fs::rename(temporary, path)?;
        Ok(())
    }

    /// File one content-distinct inquiry. An unchanged producer replay retains
    /// its receipt; independent inquiries need distinct occurrence identifiers.
    pub fn file(
        &self,
        task: &crate::queue::Task,
        run: Option<String>,
        source: String,
        summary: &str,
        detail: &str,
    ) -> Result<Inquiry> {
        let content = serde_json::to_string(&(summary, detail))?;
        let occurrence = format!("content:{}", id_for(&task.id, &source, &content));
        self.file_for_occurrence(task, run, source, &occurrence, summary, detail)
    }

    /// File or replay an explicit producer occurrence independently of prose.
    /// Use a new key for each separate inquiry, including identical wording.
    pub fn file_for_occurrence(
        &self,
        task: &crate::queue::Task,
        run: Option<String>,
        source: String,
        request: &str,
        summary: &str,
        detail: &str,
    ) -> Result<Inquiry> {
        ensure!(!request.trim().is_empty(), "inquiry occurrence is empty");
        ensure!(!summary.trim().is_empty(), "inquiry summary is empty");
        ensure!(
            run.as_ref().is_none_or(|run| task.runs.contains(run)),
            "inquiry run is not owned by its task"
        );
        let old_generation =
            serde_json::to_string(&(task.passes.len(), &task.operator_decided_at))?;
        let occurrence =
            serde_json::to_string(&(task.passes.len(), &task.operator_decided_at, request))?;
        let generation = serde_json::to_string(&(&task.repo, &run, &occurrence))?;
        let mut id = id_for(&task.id, &source, &generation);
        let clean = |s: &str| {
            crate::prompt::defuse(&crate::scrub::scrub(s, &crate::scrub::Identity::current()))
        };
        let display = format!("{}\n\n{}", clean(summary), clean(detail));
        // Adopt retained content-derived IDs without rewriting transcript links
        // or effect receipts. Unknown generations can only carry forward active,
        // unchanged intent; known generations also retain resolved history.
        if !self.path(&id)?.exists() {
            let retained: Vec<_> = self
                .list_readable()?
                .0
                .into_iter()
                .filter(|saved| {
                    saved.id.starts_with("task-")
                        && saved.task == task.id
                        && saved.run == run
                        && saved.repo == task.repo
                        && saved.source == source
                        && match &saved.occurrence {
                            Some(prior) => {
                                prior == &occurrence
                                    || (prior == &old_generation && saved.display == display)
                            }
                            None => {
                                saved.display == display
                                    && saved.decision.as_ref().is_some_and(|d| {
                                        d.status != crate::task_decision::Status::Resolved
                                    })
                            }
                        }
                })
                .collect();
            ensure!(
                retained.len() <= 1,
                "multiple inquiries claim the same producer generation"
            );
            if let Some(saved) = retained.first() {
                id = saved.id.clone();
            }
        }
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
            occurrence: Some(occurrence),
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
            // The original prose and decision lifecycle survive a producer replay.
            // Identity is ownership plus occurrence, never the rephrased body.
            if let Some(occurrence) = &inquiry.occurrence {
                let retained_generation = saved.occurrence.as_ref().is_some_and(|prior| {
                    let prior = serde_json::from_str::<Vec<serde_json::Value>>(prior);
                    let next = serde_json::from_str::<Vec<serde_json::Value>>(occurrence);
                    matches!((prior, next), (Ok(prior), Ok(next)) if prior.len() == 2 && next.len() == 3 && prior == next[..2] && saved.display == inquiry.display)
                });
                ensure!(
                    saved
                        .occurrence
                        .as_ref()
                        .is_none_or(|prior| prior == occurrence)
                        || retained_generation,
                    "inquiry identity was reused across occurrence generations"
                );
                if saved.occurrence.is_none() || retained_generation {
                    saved.occurrence = Some(occurrence.clone());
                    self.save(&saved)?;
                }
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

    /// Correct only passive compatibility imports, under delivery and execution
    /// fences. Accepted requests, explicit outcomes and delivered prose survive.
    pub(crate) fn reclassify_history(
        &self,
        incoming: Inquiry,
        resolution: HistoryResolution,
    ) -> Result<()> {
        let _execution = self.execution_claim(&incoming.id)?;
        let _lock = self.lock(&incoming.id)?;
        let mut saved = match self.get(&incoming.id) {
            Ok(saved) => saved,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return self.save(&incoming);
            }
            Err(error) => return Err(error),
        };
        ensure!(
            saved.source == incoming.source && saved.key == incoming.key,
            "history correction source identity changed"
        );
        if saved.decision.as_ref().is_some_and(|d| {
            d.request.is_some() || d.status == crate::task_decision::Status::Resolved
        }) {
            return Ok(());
        }
        ensure!(
            saved.task.is_empty()
                || incoming.task.is_empty()
                || (saved.task == incoming.task
                    && saved.repo == incoming.repo
                    && saved.run == incoming.run),
            "history correction ownership changed; reconcile the original owner"
        );
        if saved.task.is_empty() && !incoming.task.is_empty() && saved.delivered_to.is_none() {
            saved.task = incoming.task.clone();
            saved.run = incoming.run.clone();
            saved.repo = incoming.repo.clone();
        }
        // Never replace a conversation already delivered. Updating lifecycle
        // visibility does not acknowledge or redeliver its prose.
        saved.archived = incoming.archived;
        match (&mut saved.decision, incoming.decision) {
            (Some(saved), Some(next)) => {
                if next.status == crate::task_decision::Status::Resolved {
                    let evidence = next
                        .history
                        .last()
                        .context("completed history has no receipt")?;
                    if resolution == HistoryResolution::Retired {
                        saved.retire(&evidence.evidence)?;
                    } else {
                        saved.applied(&evidence.evidence)?;
                    }
                } else if let Some(error) = next.error {
                    saved.fail(&error)?;
                }
            }
            (decision @ None, next) => *decision = next,
            _ => {}
        }
        saved.delivery_error = incoming.delivery_error;
        self.save(&saved)
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
    /// Undelivered ownerless imports may be settled from any authorized chat
    /// without assigning that chat or inventing a task association. Accepted
    /// legacy choices support explicit owner confirmation of no remaining
    /// effect; native command uncertainty still requires its receipt.
    pub fn settle(
        &self,
        id: &str,
        chat: &str,
        reason: &str,
        replacement: Option<&str>,
    ) -> Result<()> {
        ensure!(!reason.trim().is_empty(), "settlement reason is empty");
        let _execution = self.execution_claim(id)?;
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
            current.delivered_to.as_deref() == Some(chat)
                || (current.task.is_empty()
                    && current.delivered_to.is_none()
                    && replacement.is_none()),
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
            format!("owner settled; no further effect remains: {reason}")
        };
        let decision = current
            .decision
            .as_mut()
            .context("inquiry is informational")?;
        let changed = if decision
            .request
            .as_deref()
            .is_some_and(|r| r.starts_with("decision: "))
        {
            decision.settle_legacy(&format!(
                "owner confirmed no remaining legacy effect: {reason}"
            ))?
        } else {
            decision.settle(&reason)?
        };
        if changed {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn execution_claim_releases_while_an_inherited_descriptor_remains_open() {
        let home = tempfile::tempdir().unwrap();
        let outbox = Outbox::at(home.path());
        let claim = outbox.execution_claim("legacy-inquiry").unwrap();
        // A fork shares this open file description until exec closes it.
        let inherited = claim._file.try_clone().unwrap();
        assert!(outbox.execution_claim("legacy-inquiry").is_err());
        drop(claim);
        let next = outbox.execution_claim("legacy-inquiry").unwrap();
        drop(inherited);
        assert!(outbox.execution_claim("legacy-inquiry").is_err());
        drop(next);
        assert!(outbox.execution_claim("legacy-inquiry").is_ok());
    }
    use crate::{
        queue::{Source, Task},
        task_decision::Status,
    };

    fn task() -> Task {
        Task::new(
            "work".into(),
            "work".into(),
            PathBuf::from("/repo"),
            Source::Human,
        )
    }

    #[test]
    fn identity_uses_task_kind_and_generation_with_unambiguous_boundaries() {
        assert_eq!(id_for("t", "k", "g"), id_for("t", "k", "g"));
        assert_ne!(id_for("t", "k", "g"), id_for("u", "k", "g"));
        assert_ne!(id_for("t", "k", "g"), id_for("t", "l", "g"));
        assert_ne!(id_for("t", "k", "g"), id_for("t", "k", "h"));
        assert_ne!(id_for("ab", "c", "d"), id_for("a", "bc", "d"));
    }

    #[test]
    fn rewording_after_restart_preserves_content_receipts_and_all_decision_states() {
        let dir = tempfile::tempdir().unwrap();
        let mut task = task();
        task.runs = vec!["run-a".into(), "run-b".into()];
        let outbox = Outbox::at(dir.path());
        let first = outbox
            .file_for_occurrence(
                &task,
                Some("run-a".into()),
                "implement".into(),
                "first-inquiry",
                "Original",
                "Original detail",
            )
            .unwrap();
        for status in [
            Status::AwaitingOwner,
            Status::Queued,
            Status::InProgress,
            Status::RetryNeeded,
            Status::Resolved,
        ] {
            let mut saved = outbox.get(&first.id).unwrap();
            saved.delivered_to = Some("chat-a".into());
            let decision = saved.decision.as_mut().unwrap();
            decision.status = status;
            decision.request =
                (status != Status::AwaitingOwner).then(|| "durable owner request".into());
            decision.error = (status == Status::RetryNeeded).then(|| "uncertain effect".into());
            outbox.save(&saved).unwrap();
            let replay = Outbox::at(dir.path())
                .file_for_occurrence(
                    &task,
                    Some("run-a".into()),
                    "implement".into(),
                    "first-inquiry",
                    "Reworded",
                    "Changed detail",
                )
                .unwrap();
            assert_eq!(replay, saved);
        }
        let other_kind = outbox
            .file_for_occurrence(
                &task,
                Some("run-a".into()),
                "review".into(),
                "first-inquiry",
                "Original",
                "Original detail",
            )
            .unwrap();
        let other_run = outbox
            .file_for_occurrence(
                &task,
                Some("run-b".into()),
                "implement".into(),
                "first-inquiry",
                "Original",
                "Original detail",
            )
            .unwrap();
        assert_ne!(first.id, other_kind.id);
        assert_ne!(first.id, other_run.id);
        task.hold_manual(Some("new owner generation".into()));
        let next = outbox
            .file_for_occurrence(
                &task,
                Some("run-a".into()),
                "implement".into(),
                "first-inquiry",
                "Original",
                "Original detail",
            )
            .unwrap();
        assert_ne!(first.id, next.id);
        assert_eq!(next.decision.unwrap().status, Status::AwaitingOwner);
        assert_eq!(outbox.list().unwrap().len(), 4);
    }

    #[test]
    fn retained_content_ids_keep_links_and_lifecycle_when_reworded() {
        let dir = tempfile::tempdir().unwrap();
        let task = task();
        let outbox = Outbox::at(dir.path());
        let mut retained = outbox
            .file_for_occurrence(
                &task,
                None,
                "implement".into(),
                "first-inquiry",
                "Original",
                "Original detail",
            )
            .unwrap();
        std::fs::remove_file(outbox.path(&retained.id).unwrap()).unwrap();
        // Both retained native formats used this namespace and content hashes.
        retained.id = "task-retained-content-hash".into();
        retained.key = format!("inquiry:{}", retained.id);
        retained.delivered_to = Some("retained-chat".into());
        retained
            .decision
            .as_mut()
            .unwrap()
            .settle("owner settled this occurrence")
            .unwrap();
        outbox.save(&retained).unwrap();
        let replay = Outbox::at(dir.path())
            .file_for_occurrence(
                &task,
                None,
                "implement".into(),
                "first-inquiry",
                "Reworded",
                "Changed detail",
            )
            .unwrap();
        assert_eq!(replay, retained);
        assert_eq!(outbox.list().unwrap().len(), 1);
    }
}
