//! Head-bound pull request merges for standing chats, carried out on the owner's
//! plain-language request.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::talk::Talks;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Request {
    pub repo: String,
    pub number: u64,
    pub head: String,
    pub request: String,
}

impl Request {
    fn validate(&self) -> Result<()> {
        let parts: Vec<_> = self.repo.split('/').collect();
        ensure!(
            parts.len() == 2
                && parts.iter().all(|s| !s.is_empty()
                    && *s != "."
                    && *s != ".."
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))),
            "expected owner/repo"
        );
        ensure!(self.number > 0, "PR number must be positive");
        ensure!(
            self.head.len() == 40 && self.head.bytes().all(|b| b.is_ascii_hexdigit()),
            "head must be a full SHA"
        );
        ensure!(
            crate::ask::valid_asset_name(&self.request),
            "invalid request ID"
        );
        Ok(())
    }

    fn url(&self) -> String {
        format!("https://github.com/{}/pull/{}", self.repo, self.number)
    }
}

/// Shared with approval publication. A busy merge must be retried by the caller,
/// never waited for while holding question locks.
pub(crate) struct TargetLock(std::fs::File);

impl Drop for TargetLock {
    fn drop(&mut self) {
        let _ = &self.0;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: the guard owns this live descriptor. Unlock even when a
            // concurrently forked child still holds an inherited descriptor.
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(crate) async fn target_lock_wait(home: &Path, url: &str) -> Result<TargetLock> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        match target_lock(home, url) {
            Ok(lock) => return Ok(lock),
            Err(e)
                if e.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    e.kind() == std::io::ErrorKind::WouldBlock
                        || cfg!(windows) && e.raw_os_error() == Some(33)
                }) && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

pub(crate) fn target_lock(home: &Path, url: &str) -> Result<TargetLock> {
    let key = canonical(url).context("unsupported PR URL")?;
    let dir = home.join("chat-merge-locks");
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(key.replace('/', "_") + ".lock"))?;
    crate::pr_head::try_lock_file(&file)?;
    Ok(TargetLock(file))
}

// A concurrent fork can inherit the open file description. Explicit unlocking
// must release the request before a retry even while that child retains it.
fn request_lock(dir: &Path, request: &str) -> Result<TargetLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(format!("{request}.lock")))?;
    crate::pr_head::try_lock_file(&file)?;
    Ok(TargetLock(file))
}

fn canonical(url: &str) -> Option<String> {
    let url = url.trim_end_matches('/').to_ascii_lowercase();
    let rest = url.strip_prefix("https://")?;
    let parts: Vec<_> = rest.split('/').collect();
    if parts.len() != 5 || parts[3] != "pull" {
        return None;
    }
    let n = parts[4].parse::<u64>().ok()?;
    Some(format!("{}/{}/{}/pull/{n}", parts[0], parts[1], parts[2]))
}

fn refuse_pending(home: &Path, url: &str) -> Result<()> {
    let root = home.join("questions");
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let q: crate::ask::Question = serde_json::from_slice(&std::fs::read(path)?)?;
        if q.node != crate::land::APPROVAL_NODE || !q.status.open() {
            continue;
        }
        let target = if let Some(t) = q.merge_target {
            t.pr
        } else {
            crate::run::RunState::load_under(&q.run, home)?
                .pr
                .context("approval has no PR")?
                .url
        };
        let target = canonical(&target).context("cannot identify pending approval PR")?;
        ensure!(
            Some(target) != canonical(url),
            "PR has pending land approval {}; answer that question through the existing approval path",
            q.id
        );
    }
    Ok(())
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    url: String,
    state: String,
    is_draft: bool,
    mergeable: String,
    head_ref_oid: String,
    title: String,
}

trait Forge {
    async fn view(&self, cwd: &Path, url: &str) -> Result<Snapshot>;
    async fn merge(&self, cwd: &Path, url: &str, head: &str, title: &str)
    -> Result<(bool, String)>;
}

struct Gh;
async fn gh(cwd: &Path, args: Vec<String>) -> Result<(bool, String)> {
    let mut command = tokio::process::Command::new("gh");
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("GH_REPO")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(60), command.output())
        .await
        .context("forge operation timed out; outcome may be unknown")??;
    Ok((
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    ))
}
impl Forge for Gh {
    async fn view(&self, cwd: &Path, url: &str) -> Result<Snapshot> {
        let (ok, body) = gh(
            cwd,
            vec![
                "pr".into(),
                "view".into(),
                url.into(),
                "--json".into(),
                "url,state,isDraft,mergeable,headRefOid,title".into(),
            ],
        )
        .await?;
        ensure!(ok, "PR lookup refused: {body}");
        Ok(serde_json::from_str(&body)?)
    }
    async fn merge(
        &self,
        cwd: &Path,
        url: &str,
        head: &str,
        title: &str,
    ) -> Result<(bool, String)> {
        gh(cwd, merge_args(url, head, title)).await
    }
}

fn merge_args(url: &str, head: &str, title: &str) -> Vec<String> {
    [
        "pr",
        "merge",
        url,
        "--squash",
        "--match-head-commit",
        head,
        "--subject",
        title,
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    action: Request,
    status: String,
    detail: String,
}
fn save(path: &Path, receipt: &Receipt) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(receipt)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub(crate) async fn merge(
    home: &Path,
    talks: &Talks,
    talk: &str,
    action: Request,
) -> Result<String> {
    merge_with(home, talks, talk, action, &Gh).await
}

async fn merge_with(
    home: &Path,
    talks: &Talks,
    talk: &str,
    action: Request,
    forge: &impl Forge,
) -> Result<String> {
    action.validate()?;
    let url = action.url();
    let _target = target_lock(home, &url)?;
    let conversation = crate::chat_control::authorize(talks, talk, crate::queue::CHAT_NODE)?;
    let dir = home.join("chat-controls").join(talk);
    std::fs::create_dir_all(&dir)?;
    let _request = request_lock(&dir, &action.request)?;
    let path = dir.join(format!("{}.json", action.request));
    let mut receipt = if path.exists() {
        let prior: Receipt = serde_json::from_slice(&std::fs::read(&path)?)?;
        ensure!(
            prior.action == action,
            "request ID already binds a different operation"
        );
        if prior.status != "started" && prior.status != "unknown" && prior.status != "pending" {
            return Ok(prior.detail);
        }
        prior
    } else {
        refuse_pending(home, &url)?;
        let seen = forge.view(&conversation.repo, &url).await?;
        ensure!(
            canonical(&seen.url) == canonical(&url),
            "forge returned a different PR"
        );
        ensure!(
            seen.state == "OPEN" && !seen.is_draft && seen.mergeable == "MERGEABLE",
            "PR must be open, not draft, and mergeable"
        );
        ensure!(
            seen.head_ref_oid.eq_ignore_ascii_case(&action.head),
            "PR head differs from the given SHA"
        );
        ensure!(
            !seen.title.trim().is_empty() && !seen.title.contains(['\n', '\r', '\0']),
            "invalid PR title"
        );
        let mut rec = Receipt {
            action,
            status: "started".into(),
            detail: "merge started; outcome unknown".into(),
        };
        save(&path, &rec)?;
        match forge
            .merge(&conversation.repo, &url, &rec.action.head, &seen.title)
            .await
        {
            Ok((true, _)) => {
                // A zero exit can mean queued or auto-merge enabled. Persist
                // acceptance, then require a forge-confirmed MERGED lifecycle.
                rec.status = "pending".into();
                rec.detail = "forge accepted merge command; completion not confirmed".into();
                save(&path, &rec)?;
            }
            Ok((false, output)) => {
                rec.detail = format!("forge refused or outcome unknown: {output}")
            }
            Err(e) => rec.detail = format!("merge outcome unknown: {e:#}"),
        }
        rec
    };
    match forge.view(&conversation.repo, &url).await {
        Ok(after)
            if canonical(&after.url) == canonical(&url)
                && after.state == "MERGED"
                && after
                    .head_ref_oid
                    .eq_ignore_ascii_case(&receipt.action.head) =>
        {
            receipt.status = "merged".into();
            receipt.detail = format!("merged {url} at head {}", receipt.action.head);
        }
        Ok(after)
            if canonical(&after.url) == canonical(&url)
                && after.state == "OPEN"
                && receipt.status == "pending" =>
        {
            receipt.detail = format!(
                "merge pending for {url}; forge still reports OPEN; reuse this request ID to check only"
            );
        }
        Ok(after) if canonical(&after.url) == canonical(&url) && after.state != "MERGED" => {
            receipt.status = "refused".into();
            receipt.detail = format!(
                "merge not applied ({}) : {}; no retry will be issued",
                after.state, receipt.detail
            );
        }
        _ => {
            if receipt.status != "pending" {
                receipt.status = "unknown".into();
            }
            receipt.detail = format!(
                "outcome unknown; reuse this request ID to check only: {}",
                receipt.detail
            );
        }
    }
    save(&path, &receipt)?;
    Ok(receipt.detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn request() -> Request {
        Request {
            repo: "team/project".into(),
            number: 7,
            head: "a".repeat(40),
            request: "merge-1".into(),
        }
    }
    fn fixture(words: &str) -> (tempfile::TempDir, Talks) {
        let home = tempfile::tempdir().unwrap();
        let talks = Talks::at(home.path().join("talks"));
        let mut talk: crate::talk::Talk = serde_json::from_value(serde_json::json!({
            "schema":1, "id":"talk-a", "repo":home.path(), "agent":"worker", "status":"open", "turns":[],
            "created_at":jiff::Timestamp::now(), "updated_at":jiff::Timestamp::now(),
            "seat":crate::agent::SeatState::new("talk", "worker", 1)
        })).unwrap();
        talks.put(&mut talk).unwrap();
        if !words.is_empty() {
            crate::talk::record(&mut talk, &talks, words, vec![]).unwrap();
        }
        (home, talks)
    }
    fn snapshot() -> Snapshot {
        Snapshot {
            url: request().url(),
            state: "OPEN".into(),
            is_draft: false,
            mergeable: "MERGEABLE".into(),
            head_ref_oid: request().head,
            title: "chore(deps): update dependency".into(),
        }
    }
    fn merged_snapshot() -> Snapshot {
        let mut seen = snapshot();
        seen.state = "MERGED".into();
        seen
    }
    struct Fake {
        views: Mutex<VecDeque<Result<Snapshot>>>,
        calls: Mutex<usize>,
        success: bool,
        receipt: std::path::PathBuf,
    }
    impl Fake {
        fn new(home: &Path, views: Vec<Result<Snapshot>>, success: bool) -> Self {
            Self {
                views: Mutex::new(views.into()),
                calls: Mutex::new(0),
                success,
                receipt: home.join("chat-controls/talk-a/merge-1.json"),
            }
        }
    }
    impl Forge for Fake {
        async fn view(&self, _: &Path, _: &str) -> Result<Snapshot> {
            self.views
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected forge read")
        }
        async fn merge(
            &self,
            _: &Path,
            url: &str,
            head: &str,
            title: &str,
        ) -> Result<(bool, String)> {
            let receipt: Receipt = serde_json::from_slice(&std::fs::read(&self.receipt)?).unwrap();
            assert_eq!(receipt.status, "started");
            assert_eq!(url, request().url());
            assert_eq!(head, request().head);
            assert_eq!(title, snapshot().title);
            *self.calls.lock().unwrap() += 1;
            Ok((self.success, "forge output".into()))
        }
    }
    async fn apply(home: &Path, talks: &Talks, fake: &Fake) -> Result<String> {
        merge_with(home, talks, "talk-a", request(), fake).await
    }

    #[tokio::test]
    async fn plain_requests_merge_without_a_quote() {
        for words in ["merge project 7", "マージして"] {
            let (home, talks) = fixture(words);
            let fake = Fake::new(
                home.path(),
                vec![Ok(snapshot()), Ok(merged_snapshot())],
                true,
            );
            assert!(
                apply(home.path(), &talks, &fake)
                    .await
                    .unwrap()
                    .starts_with("merged ")
            );
            assert_eq!(*fake.calls.lock().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn later_and_queued_input_and_consultations_do_not_block_merging() {
        for queued in [false, true] {
            let (home, talks) = fixture("merge project 7");
            let mut talk = talks.get("talk-a").unwrap();
            if queued {
                talk.pending = "another request".into();
                talks.put(&mut talk).unwrap();
            } else {
                crate::talk::record(&mut talk, &talks, "thanks", vec![]).unwrap();
            }
            let questions = crate::ask::Questions::at(home.path().join("questions"));
            let mut q = crate::ask::Question::new(
                "run".into(),
                "other".into(),
                "seat".into(),
                "Question?".into(),
                String::new(),
                vec!["yes".into()],
            );
            questions.put(&mut q).unwrap();
            assert!(crate::consult::begin(&questions, &talks, &q, &talk).unwrap());
            assert!(crate::consult::pending_consults(&questions, "talk-a"));
            let fake = Fake::new(
                home.path(),
                vec![Ok(snapshot()), Ok(merged_snapshot())],
                true,
            );
            assert!(
                apply(home.path(), &talks, &fake)
                    .await
                    .unwrap()
                    .starts_with("merged ")
            );
        }
    }

    #[tokio::test]
    async fn only_an_open_standing_chat_can_merge() {
        let (home, talks) = fixture("merge project 7");
        assert!(crate::chat_control::authorize(&talks, "talk-a", "worker").is_err());
        assert!(crate::chat_control::authorize(&talks, "talk", crate::queue::CHAT_NODE).is_err());
        assert!(crate::chat_control::authorize(&talks, "", crate::queue::CHAT_NODE).is_err());
        let mut talk = talks.get("talk-a").unwrap();
        talk.status = crate::talk::TalkStatus::Closed;
        talks.put(&mut talk).unwrap();
        let fake = Fake::new(home.path(), vec![], true);
        assert!(apply(home.path(), &talks, &fake).await.is_err());
        assert_eq!(*fake.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn head_mismatch_draft_unknown_and_wrong_pr_are_refused() {
        for variant in 0..7 {
            let (home, talks) = fixture("マージして");
            let mut seen = snapshot();
            match variant {
                0 => seen.head_ref_oid = "b".repeat(40),
                1 => seen.is_draft = true,
                2 => seen.mergeable = "UNKNOWN".into(),
                3 => seen.state = "CLOSED".into(),
                4 => seen.url = "https://github.com/team/project/pull/8".into(),
                5 => seen.mergeable = "CONFLICTING".into(),
                _ => seen.title = "bad\nsubject".into(),
            }
            let fake = Fake::new(home.path(), vec![Ok(seen)], true);
            assert!(apply(home.path(), &talks, &fake).await.is_err());
            assert!(!fake.receipt.exists());
        }
    }

    #[tokio::test]
    async fn happy_path_records_receipt_and_retry_never_merges_twice() {
        let (home, talks) = fixture("マージして");
        let fake = Fake::new(
            home.path(),
            vec![Ok(snapshot()), Ok(merged_snapshot())],
            true,
        );
        let first = apply(home.path(), &talks, &fake).await.unwrap();
        assert!(first.starts_with("merged "));
        assert_eq!(apply(home.path(), &talks, &fake).await.unwrap(), first);
        assert_eq!(*fake.calls.lock().unwrap(), 1);
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&fake.receipt).unwrap()).unwrap();
        assert_eq!(receipt.action, request());
        assert_eq!(receipt.status, "merged");
    }

    #[tokio::test]
    async fn successful_command_that_only_queues_is_pending_until_confirmed() {
        let (home, talks) = fixture("マージして");
        let fake = Fake::new(
            home.path(),
            vec![Ok(snapshot()), Ok(snapshot()), Ok(merged_snapshot())],
            true,
        );
        let first = apply(home.path(), &talks, &fake).await.unwrap();
        assert!(first.starts_with("merge pending"));
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(&fake.receipt).unwrap()).unwrap();
        assert_eq!(receipt.status, "pending");
        assert!(
            apply(home.path(), &talks, &fake)
                .await
                .unwrap()
                .starts_with("merged ")
        );
        assert_eq!(*fake.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn accepted_command_with_unknown_confirmation_stays_read_only() {
        let (home, talks) = fixture("マージして");
        let fake = Fake::new(
            home.path(),
            vec![
                Ok(snapshot()),
                Err(anyhow::anyhow!("unavailable")),
                Ok(snapshot()),
                Ok(merged_snapshot()),
            ],
            true,
        );
        assert!(
            apply(home.path(), &talks, &fake)
                .await
                .unwrap()
                .contains("outcome unknown")
        );
        assert!(
            apply(home.path(), &talks, &fake)
                .await
                .unwrap()
                .starts_with("merge pending")
        );
        assert!(
            apply(home.path(), &talks, &fake)
                .await
                .unwrap()
                .starts_with("merged ")
        );
        assert_eq!(*fake.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn queued_command_removed_without_merging_is_never_reported_merged() {
        let (home, talks) = fixture("マージして");
        let mut closed = snapshot();
        closed.state = "CLOSED".into();
        let fake = Fake::new(
            home.path(),
            vec![Ok(snapshot()), Ok(snapshot()), Ok(closed)],
            true,
        );
        assert!(
            apply(home.path(), &talks, &fake)
                .await
                .unwrap()
                .starts_with("merge pending")
        );
        let result = apply(home.path(), &talks, &fake).await.unwrap();
        assert!(result.contains("merge not applied (CLOSED)"));
        assert_eq!(apply(home.path(), &talks, &fake).await.unwrap(), result);
        assert_eq!(*fake.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn refused_merge_is_confirmed_and_never_reissued() {
        let (home, talks) = fixture("マージして");
        let fake = Fake::new(home.path(), vec![Ok(snapshot()), Ok(snapshot())], false);
        let first = apply(home.path(), &talks, &fake).await.unwrap();
        assert!(first.contains("merge not applied"));
        assert_eq!(apply(home.path(), &talks, &fake).await.unwrap(), first);
        assert_eq!(*fake.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn nonzero_exit_after_success_and_uncertain_retry_use_reads_only() {
        for uncertain in [false, true] {
            let (home, talks) = fixture("マージして");
            let mut merged = snapshot();
            merged.state = "MERGED".into();
            let mut views = vec![Ok(snapshot())];
            if uncertain {
                views.push(Err(anyhow::anyhow!("lookup unavailable")));
            }
            views.push(Ok(merged));
            let fake = Fake::new(home.path(), views, false);
            let first = apply(home.path(), &talks, &fake).await.unwrap();
            if uncertain {
                assert!(first.contains("outcome unknown"));
            } else {
                assert!(first.starts_with("merged "));
            }
            assert!(
                apply(home.path(), &talks, &fake)
                    .await
                    .unwrap()
                    .starts_with("merged ")
            );
            assert_eq!(*fake.calls.lock().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn interrupted_started_receipt_is_reconciled_without_reissuing_merge() {
        let (home, talks) = fixture("マージして");
        let fake = Fake::new(home.path(), vec![Ok(snapshot())], false);
        std::fs::create_dir_all(fake.receipt.parent().unwrap()).unwrap();
        // Receipts from earlier versions may still carry owner evidence.
        std::fs::write(
            &fake.receipt,
            serde_json::to_vec(&serde_json::json!({
                "action": request(), "evidence": {"instruction": "legacy"},
                "status": "started", "detail": "interrupted"
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            apply(home.path(), &talks, &fake)
                .await
                .unwrap()
                .contains("merge not applied")
        );
        assert_eq!(*fake.calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn pending_approval_and_unreadable_records_fail_closed() {
        for corrupt in [false, true] {
            let (home, talks) = fixture("マージして");
            let store = crate::ask::Questions::at(home.path().join("questions"));
            let mut q = crate::ask::Question::new(
                "run".into(),
                crate::land::APPROVAL_NODE.into(),
                "land".into(),
                "Merge?".into(),
                String::new(),
                vec!["merge".into()],
            );
            q.merge_target = Some(crate::merge_decision::Target {
                pr: "https://github.com/TEAM/PROJECT/pull/7/".into(),
                head: request().head,
                language: "en".into(),
            });
            store.put(&mut q).unwrap();
            if corrupt {
                std::fs::write(store.path_of(&q.id), "broken").unwrap();
            }
            let fake = Fake::new(home.path(), vec![], true);
            assert!(apply(home.path(), &talks, &fake).await.is_err());
            assert_eq!(*fake.calls.lock().unwrap(), 0);
        }
    }

    #[tokio::test]
    async fn request_collision_cannot_change_target() {
        let (home, talks) = fixture("マージして");
        let fake = Fake::new(
            home.path(),
            vec![Ok(snapshot()), Ok(merged_snapshot())],
            true,
        );
        apply(home.path(), &talks, &fake).await.unwrap();
        let mut changed = request();
        changed.number = 8;
        assert!(
            merge_with(home.path(), &talks, "talk-a", changed, &fake)
                .await
                .is_err()
        );
        assert_eq!(*fake.calls.lock().unwrap(), 1);
    }

    #[test]
    fn merge_arguments_are_head_bound_squash_with_the_literal_title() {
        let title = "chore(deps): update (literal $text)";
        assert_eq!(
            merge_args(
                "https://github.com/team/project/pull/7",
                &"a".repeat(40),
                title
            ),
            vec![
                "pr",
                "merge",
                "https://github.com/team/project/pull/7",
                "--squash",
                "--match-head-commit",
                &"a".repeat(40),
                "--subject",
                title
            ]
        );
        for invalid in ["team/project/extra", "../project", "-team/project;cmd"] {
            let mut action = request();
            action.repo = invalid.into();
            assert!(action.validate().is_err());
        }
        let mut action = request();
        action.head = "abc".into();
        assert!(action.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn request_lock_releases_even_while_an_inherited_descriptor_is_open() {
        let home = tempfile::tempdir().unwrap();
        let first = request_lock(home.path(), "merge-1").unwrap();
        // A clone shares the file description just as a concurrent fork does.
        let inherited = first.0.try_clone().unwrap();
        assert!(request_lock(home.path(), "merge-1").is_err());
        drop(first);
        let retry = request_lock(home.path(), "merge-1").unwrap();
        drop(inherited);
        assert!(request_lock(home.path(), "merge-1").is_err());
        drop(retry);
        assert!(request_lock(home.path(), "merge-1").is_ok());
    }

    #[test]
    fn target_lock_serializes_approval_publication_and_chat_merge() {
        let home = tempfile::tempdir().unwrap();
        let guard = target_lock(home.path(), &request().url()).unwrap();
        assert!(target_lock(home.path(), "https://github.com/TEAM/PROJECT/pull/7/").is_err());
        drop(guard);
        assert!(target_lock(home.path(), &request().url()).is_ok());
    }
}
