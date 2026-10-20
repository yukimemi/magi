//! Task retries publish one stable head while candidates remain per run.
#![cfg(unix)]
mod common;

use common::{Judges, fixture, home_lock};
use magi::{
    config::MergeMode,
    git,
    graph::Runner,
    run::{Origin, RunState},
};
use std::path::Path;

common::e2e! {
async fn common_change_summary_mock_preserves_head_and_working_tree() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    std::fs::write(fx.repo.join("note.txt"), "pending work\n").unwrap();
    assert!(std::process::Command::new("git")
        .args(["add", "note.txt"])
        .current_dir(&fx.repo)
        .status().unwrap().success());
    let snapshot = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args).current_dir(&fx.repo).output().unwrap();
        assert!(out.status.success());
        out.stdout
    };
    let head = snapshot(&["rev-parse", "HEAD"]);
    let status = snapshot(&["status", "--porcelain"]);
    let staged = snapshot(&["diff", "--cached"]);
    let prompt_file = fx.tmp.path().join("summary-prompt.md");
    for subject in ["feat: add note", "fix: adjust note after review"] {
        let prompt = magi::prompt::change_summary(
            "origin/main", "HEAD", &[subject.into()], "note.txt | 1 +",
        );
        std::fs::write(&prompt_file, prompt).unwrap();
        let reply = std::process::Command::new("sh")
            .arg(fx.tmp.path().join("mock-agent.sh"))
            .env("MAGI_PROMPT_FILE", &prompt_file)
            .env("MAGI_SEAT", "summary")
            .env("MAGI_NODE", "change-summary")
            .current_dir(&fx.repo).output().unwrap();
        assert!(reply.status.success(), "{:?}", reply);
        assert_eq!(String::from_utf8(reply.stdout).unwrap(),
            "TITLE: feat(note): create note.txt\nAdds `note.txt` with the seat output.\n");
        assert_eq!(snapshot(&["rev-parse", "HEAD"]), head);
        assert_eq!(snapshot(&["status", "--porcelain"]), status);
        assert_eq!(snapshot(&["diff", "--cached"]), staged);
        assert_eq!(std::fs::read_to_string(fx.repo.join("note.txt")).unwrap(), "pending work\n");
    }
}
}

fn forge(dir: &Path) -> std::path::PathBuf {
    let bin = dir.join("forge");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("gh");
    std::fs::write(&script, r#"#!/bin/sh
printf '%s\n' "$1 $2" >> "$TASK_PR_FORGE/calls"
case "$1 $2" in
  'pr list')
    if test -f "$TASK_PR_FORGE/open"; then
      echo '[{"number":1,"url":"https://github.com/example/repo/pull/1","title":"Human title","baseRefName":"main"}]'
    else echo '[]'; fi ;;
  'pr view')
    sha=$(git ls-remote --refs origin refs/heads/magi/task-abcd-1234 2>/dev/null | cut -f1)
    lifecycle=OPEN
    if test -f "$TASK_PR_FORGE/merged"; then lifecycle=MERGED; fi
    printf '{"url":"https://github.com/example/repo/pull/1","number":1,"title":"feat: existing work","reviews":[],"comments":[],"mergeStateStatus":"CLEAN","autoMergeRequest":null,"headRefName":"magi/task-abcd-1234","baseRefName":"main","isCrossRepository":false,"headRefOid":"%s","state":"%s"}\n' "$sha" "$lifecycle" ;;
  'pr create') touch "$TASK_PR_FORGE/open"; echo 'https://github.com/example/repo/pull/1' ;;
  'pr edit') test ! -f "$TASK_PR_FORGE/fail-edit" || exit 1; printf '%s\n' "$@" >> "$TASK_PR_FORGE/edits" ;;
  'api graphql')
    sha=$(git ls-remote --refs origin refs/heads/magi/task-abcd-1234 2>/dev/null | cut -f1)
    printf '{"data":{"repository":{"pullRequest":{"commits":{"nodes":[{"commit":{"oid":"%s","statusCheckRollup":{"contexts":{"pageInfo":{"hasNextPage":false},"nodes":[{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS"}]}}}}]}}}}}\n' "$sha" ;;
  'api repos/{owner}/{repo}/pulls/1/comments') echo '[]' ;;
  'pr merge') printf '%s\n' "$*" >> "$TASK_PR_FORGE/merges"; case "$*" in *--disable-auto*) : ;; *) test -f "$TASK_PR_FORGE/allow-merge" || exit 99; touch "$TASK_PR_FORGE/merged" ;; esac ;;
  'pr comment') : ;;
  *) exit 1 ;;
esac
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    // SAFETY: e2e starts each test in a separate process before other work.
    unsafe {
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        std::env::set_var("TASK_PR_FORGE", &bin);
    }
    bin
}

async fn attempt(repo: &Path, config: &magi::config::Config) -> RunState {
    let mut runner = Runner::start(
        repo,
        "create note.txt".into(),
        config.clone(),
        Origin::queue("abcd-1234"),
    )
    .await
    .unwrap();
    runner.execute().await.unwrap();
    assert!(
        runner.state.merge.as_ref().unwrap().ok,
        "{:?}",
        runner.state.events
    );
    assert_eq!(runner.state.status, magi::run::RunStatus::Ready);
    runner.state
}

common::e2e! {
async fn retry_public_head_updates_the_existing_open_pull_request() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    let mut stale = Runner::resume(&first.id).unwrap();
    let second = attempt(&fx.repo, &fx.config).await;
    assert!(stale.execute().await.is_err(), "even an in-memory predecessor cannot resume");
    assert_ne!(first.winner().unwrap().branch, second.winner().unwrap().branch);
    assert_eq!(second.pr_branch(), Some("magi/task-abcd-1234"));
    assert_eq!(first.pr.as_ref().unwrap().number, second.pr.as_ref().unwrap().number);
    let old = RunState::load(&first.id).unwrap();
    assert_eq!(old.pr_taken_over_by.as_deref(), Some(second.id.as_str()));
    let error = Runner::resume(&old.id).err().expect("a superseded run cannot reopen");
    assert!(
        error.to_string().contains("pull request ownership was superseded"),
        "{error:#}",
    );
    let calls = std::fs::read_to_string(forge.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| *l == "pr create").count(), 1);
    assert_eq!(calls.lines().filter(|l| *l == "pr edit").count(), 1);
    assert_eq!(calls.lines().filter(|l| *l == "pr comment").count(), 1);
    let edits = std::fs::read_to_string(forge.join("edits")).unwrap();
    assert!(edits.lines().any(|l| l == "--body"));
    assert!(!edits.lines().any(|l| l == "--title"), "preserve human title");
    assert_eq!(git::remote_head(&fx.repo, "origin", second.pr_branch().unwrap()).await.unwrap(), second.pr_pushed_head);
    for candidate in &second.candidates {
        assert!(!second.pr_branch().unwrap().contains(&candidate.agent));
        assert!(!second.pr_branch().unwrap().ends_with(&format!("/{}", candidate.label)));
    }
}
}

common::e2e! {
async fn retry_public_head_creates_a_pull_request_when_none_is_open() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    std::fs::remove_file(forge.join("open")).unwrap();
    let second = attempt(&fx.repo, &fx.config).await;
    assert_eq!(first.pr_branch(), second.pr_branch());
    let calls = std::fs::read_to_string(forge.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| *l == "pr create").count(), 2);
    assert_eq!(calls.lines().filter(|l| *l == "pr edit").count(), 0);
}
}

common::e2e! {
async fn a_moved_public_head_is_not_overwritten_even_after_fetch() {
    let home = home_lock().await;
    let fx = fixture(home, Judges::Unanimous, false);
    let head = "magi/task-abcd-1234";
    let initial = git::rev_parse(&fx.repo, "HEAD").await.unwrap();
    assert!(git::push_as(&fx.repo, "origin", "main", head, "").await.unwrap().ok());
    std::fs::write(fx.repo.join("human.txt"), "human change").unwrap();
    git::git(&fx.repo, &["add", "human.txt"]).await.unwrap();
    git::git(&fx.repo, &["commit", "-m", "test: move public head"]).await.unwrap();
    let moved = git::rev_parse(&fx.repo, "HEAD").await.unwrap();
    assert!(git::push_as(&fx.repo, "origin", "main", head, &initial).await.unwrap().ok());
    git::fetch(&fx.repo, "origin", head).await.unwrap();
    git::git(&fx.repo, &["branch", "replacement", &initial]).await.unwrap();
    assert!(!git::push_as(&fx.repo, "origin", "replacement", head, &initial).await.unwrap().ok());
    assert_eq!(git::remote_head(&fx.repo, "origin", head).await.unwrap().as_deref(), Some(moved.as_str()));
    assert!(!git::push_as(&fx.repo, "origin", "replacement", head, "").await.unwrap().ok());
}
}

common::e2e! {
async fn public_head_metadata_failure_resumes_without_competing_again() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    std::fs::write(forge.join("fail-edit"), "").unwrap();
    let mut second = Runner::start(&fx.repo, "create note.txt".into(), fx.config.clone(), Origin::queue("abcd-1234")).await.unwrap();
    second.execute().await.unwrap();
    assert!(!second.state.merge.as_ref().unwrap().ok);
    assert!(second.state.pr_publication_pending);
    assert!(RunState::load(&first.id).unwrap().pr_taken_over_by.is_none());
    let branch = second.state.winner().unwrap().branch.clone();
    let reviews = second.state.reviews.len();
    let verified_tip = git::rev_parse(&fx.repo, &branch).await.unwrap();
    let synced_base = second.state.base_sync.clone();
    // Advance the remote base after verification and the failed metadata call.
    // Publication retry must not rebase the candidate under its old checks.
    std::fs::write(fx.repo.join("base-update.txt"), "new base").unwrap();
    git::git(&fx.repo, &["add", "base-update.txt"]).await.unwrap();
    git::git(&fx.repo, &["commit", "-m", "test: advance base during publication retry"]).await.unwrap();
    git::git(&fx.repo, &["push", "origin", "main"]).await.unwrap();
    std::fs::remove_file(forge.join("fail-edit")).unwrap();
    let mut resumed = Runner::resume(&second.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.merge.as_ref().unwrap().ok, "{:?}", resumed.state.events);
    assert!(!resumed.state.pr_publication_pending);
    assert_eq!(resumed.state.winner().unwrap().branch, branch);
    assert_eq!(resumed.state.reviews.len(), reviews);
    assert_eq!(git::rev_parse(&fx.repo, &branch).await.unwrap(), verified_tip);
    assert_eq!(resumed.state.pr_pushed_head.as_deref(), Some(verified_tip.as_str()));
    assert_eq!(serde_json::to_value(&resumed.state.base_sync).unwrap(), serde_json::to_value(&synced_base).unwrap());
    let calls = std::fs::read_to_string(forge.join("calls")).unwrap();
    assert_eq!(calls.lines().filter(|l| *l == "pr create").count(), 1);
    assert_eq!(RunState::load(&first.id).unwrap().pr_taken_over_by.as_deref(), Some(resumed.state.id.as_str()));
}
}

common::e2e! {
async fn driver_claim_and_publication_takeover_share_an_ownership_lock() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    forge(fx.tmp.path());
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
    let first = attempt(&fx.repo, &fx.config).await;
    let mut stale = Runner::resume(&first.id).unwrap();
    let lock = std::fs::OpenOptions::new().read(true).write(true)
        .open(first.dir().join("pr-owner.lock")).unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: lock owns a live descriptor; closing it releases the lock.
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    // Model the takeover's critical section: a stale driver cannot claim or
    // save while publication owns the old record.
    let error = stale.execute().await.unwrap_err();
    assert!(format!("{error:#}").contains("ownership is being updated"));
    assert!(RunState::load(&first.id).unwrap().driver_exited);
    let mut second = Runner::start(&fx.repo, "create note.txt".into(), fx.config.clone(), Origin::queue("abcd-1234")).await.unwrap();
    second.execute().await.unwrap();
    assert!(!second.state.merge.as_ref().unwrap().ok);
    assert_eq!(git::remote_head(&fx.repo, "origin", first.pr_branch().unwrap()).await.unwrap(), first.pr_pushed_head);
    drop(lock);
    let mut resumed = Runner::resume(&second.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.merge.as_ref().unwrap().ok);
    // Even the runner whose first check raced publication must now reject
    // the disk takeover without saving its old in-memory state over it.
    assert!(stale.execute().await.is_err());
    assert_eq!(RunState::load(&first.id).unwrap().pr_taken_over_by.as_deref(), Some(resumed.state.id.as_str()));
}
}

fn pr_config(fx: &mut common::Fixture) {
    fx.config.merge.mode = MergeMode::Pr;
    fx.config.graph.land = false;
    fx.config.graph.implementers = 1;
    fx.config.graph.reviewers = 1;
}

fn comments(forge: &Path) -> usize {
    std::fs::read_to_string(forge.join("calls"))
        .unwrap()
        .lines()
        .filter(|l| *l == "pr comment")
        .count()
}

common::e2e! {
async fn a_merged_predecessor_is_history_and_does_not_block_a_new_pull_request() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    pr_config(&mut fx);
    let first = attempt(&fx.repo, &fx.config).await;
    let mut merged = RunState::load(&first.id).unwrap();
    merged.pr.as_mut().unwrap().state = "merged".into();
    merged.save().unwrap();
    std::fs::remove_file(forge.join("open")).unwrap();
    let second = attempt(&fx.repo, &fx.config).await;
    let old = RunState::load(&first.id).unwrap();
    assert!(old.pr_taken_over_by.is_none());
    assert_eq!(old.pr.as_ref().unwrap().state, "merged");
    assert_eq!(old.status, first.status);
    assert_eq!(comments(&forge), 0);
    assert_eq!(git::remote_head(&fx.repo, "origin", second.pr_branch().unwrap()).await.unwrap(), second.pr_pushed_head);
}
}

common::e2e! {
async fn a_predecessor_with_another_pull_request_is_not_superseded() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    pr_config(&mut fx);
    let first = attempt(&fx.repo, &fx.config).await;
    let mut other = RunState::load(&first.id).unwrap();
    other.pr.as_mut().unwrap().url = "https://github.com/example/repo/pull/99".into();
    other.pr.as_mut().unwrap().number = 99;
    other.save().unwrap();
    attempt(&fx.repo, &fx.config).await;
    let old = RunState::load(&first.id).unwrap();
    assert!(old.pr_taken_over_by.is_none());
    assert_eq!(old.pr.as_ref().unwrap().number, 99);
    assert_eq!(comments(&forge), 0);
}
}

common::e2e! {
async fn lease_survives_a_merged_run_when_a_later_run_takes_over_the_open_pull_request() {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let forge = forge(fx.tmp.path());
    pr_config(&mut fx);
    let first = attempt(&fx.repo, &fx.config).await;
    let mut merged = RunState::load(&first.id).unwrap();
    merged.pr.as_mut().unwrap().state = "merged".into();
    merged.save().unwrap();
    std::fs::remove_file(forge.join("open")).unwrap();
    // A new pull request on the existing head: the lease comes from the merged run's push.
    let second = attempt(&fx.repo, &fx.config).await;
    // The merged record is touched later (write-through / repair) and must not matter.
    let mut touched = RunState::load(&first.id).unwrap();
    touched.event("merge", "repaired");
    touched.save().unwrap();
    let third = attempt(&fx.repo, &fx.config).await;
    assert!(RunState::load(&first.id).unwrap().pr_taken_over_by.is_none());
    assert_eq!(RunState::load(&second.id).unwrap().pr_taken_over_by.as_deref(), Some(third.id.as_str()));
    assert_eq!(comments(&forge), 1);
    assert_eq!(git::remote_head(&fx.repo, "origin", third.pr_branch().unwrap()).await.unwrap(), third.pr_pushed_head);
}
}

// The local bare remote transports real Git objects; the wrapper supplies its
// forge identity without substituting any ownership, ancestor or lease checks.
fn recovery_transport(bin: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let real = std::process::Command::new("which")
        .arg("git")
        .output()
        .unwrap();
    assert!(real.status.success());
    let script = bin.join("git");
    std::fs::write(
        &script,
        r#"#!/bin/sh
if test "$1 $2" = 'remote get-url'; then
  echo 'https://github.com/example/repo.git'
else exec "$RECOVERY_REAL_GIT" "$@"; fi
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: isolated test process before recovery dispatch.
    unsafe {
        std::env::set_var(
            "RECOVERY_REAL_GIT",
            String::from_utf8(real.stdout).unwrap().trim(),
        );
    }
}

async fn selected_fixture() -> (
    common::Fixture,
    magi::queue::Task,
    magi::pr_recovery::Selection,
    std::path::PathBuf,
) {
    let home = home_lock().await;
    let mut fx = fixture(home, Judges::Unanimous, false);
    let bin = forge(fx.tmp.path());
    // Shell mocks create tiny fixtures. Keep disk preflight enabled without
    // inheriting the production workload's 8 GiB requirement from the host.
    fx.config.disk.min_free_bytes = 1024 * 1024;
    pr_config(&mut fx);
    let first = attempt(&fx.repo, &fx.config).await;
    let public = first.pr_pushed_head.clone().unwrap();
    let wt = &first.winner().unwrap().worktree;
    std::fs::write(wt.join("reviewed-fix.txt"), "preserved fix").unwrap();
    git::git(wt, &["add", "reviewed-fix.txt"]).await.unwrap();
    git::git(
        wt,
        &["commit", "-m", "fix: preserve reviewed implementation"],
    )
    .await
    .unwrap();
    let source = git::rev_parse(wt, "HEAD").await.unwrap();
    recovery_transport(&bin);
    let mut task = magi::queue::Task::new(
        "existing delivery".into(),
        "create note.txt".into(),
        fx.repo.clone(),
        magi::queue::Source::Human,
    );
    task.id = "abcd-1234".into();
    task.runs.push(first.id);
    task.succeed();
    task.overrides = Some(magi::queue::RunOverrides {
        merge: Some("none".into()),
        reviewers: Some(1),
        ..Default::default()
    });
    let selected = magi::pr_recovery::Selection {
        url: "https://github.com/example/repo/pull/1".into(),
        branch: "magi/task-abcd-1234".into(),
        head: public,
        source_head: Some(source),
        source_run: None,
        source_proof: None,
        disposition: Some(magi::pr_recovery::Disposition::Land),
        instruction: task.instruction.clone(),
        approved_at: jiff::Timestamp::now(),
    };
    (fx, task, selected, bin)
}

common::e2e! {
async fn selected_delivery_reconciles_review_override_preserves_fix_and_waits_for_fresh_approval() {
    let (fx, mut task, selected, bin) = selected_fixture().await;
    let queue = magi::queue::Queue::open();
    queue.put(&mut task).unwrap();
    let config_path = fx.tmp.path().join("recovery.toml");
    std::fs::write(&config_path, toml::to_string(&fx.config).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["task", "recover-pr", &task.id, "--pr", &selected.url, "--head", &selected.head,
            "--source-head", selected.source(), "--disposition", "land", "--config"])
        .arg(&config_path).env("MAGI_HOME", magi::run::home()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("replaced explicitly"));
    let mut task = queue.get(&task.id).unwrap();
    assert_eq!(task.overrides.as_ref().unwrap().merge.as_deref(), Some("pr"));
    assert_eq!(task.overrides.as_ref().unwrap().reviewers, Some(1));
    let expected_source = selected.source_head.clone();
    let selected = task.pr_recovery.clone().unwrap();
    assert_eq!(selected.source_head, expected_source);
    let (mut config, layers) = magi::config::Config::discover_fetched(&fx.repo, Some(&config_path)).await.unwrap();
    task.overrides.as_ref().unwrap().apply(&mut config);
    assert_eq!(layers, vec![config_path.clone()]);
    let opts = magi::daemon::Opts { repo: fx.repo.clone(), config: Some(fx.tmp.path().join("ignored-daemon.toml")), merge: Some("none".into()), ..Default::default() };
    Box::pin(magi::daemon::run_claimed(&opts, &queue, &mut task)).await;
    let runner = Runner::resume(task.runs.last().unwrap()).unwrap();
    assert!(runner.state.config.graph.land);
    assert!(runner.state.config.graph.land_approval);
    let provenance = runner.state.recovery_config.as_ref().unwrap();
    assert_eq!(provenance.layers, vec![config_path]);
    assert!(provenance.daemon_merge.is_none());
    assert_eq!(runner.state.config.verify.gate, config.verify.gate);
    assert_eq!(runner.state.config.verify.e2e, config.verify.e2e);
    assert_eq!(git::rev_parse(&runner.state.winner().unwrap().worktree, "HEAD").await.unwrap(), selected.source());
    let result = Ok(());
    assert!(runner.state.parked, "{:?}", runner.state.events);
    assert!(!runner.state.public_delivery_complete());
    for status in [magi::run::RunStatus::Ready, magi::run::RunStatus::Merged] {
        let mut snapshot = runner.state.clone();
        snapshot.parked = false;
        snapshot.status = status;
        let mut unfinished = task.clone();
        magi::daemon::finish_attempt(3, &queue, &mut unfinished, &snapshot, &[], Ok(()));
        assert_eq!(unfinished.status, magi::queue::TaskStatus::Held);
    }
    let approval = runner.state.land_approval.clone().expect("fresh approval");
    let head = runner.state.pr_pushed_head.clone().unwrap();
    assert_eq!(approval.head, head);
    assert_ne!(head, selected.head);
    runner.state.check_delivery_evidence(&head).unwrap();
    for stale in ["gate", "e2e", "review", "missing-commands", "missing-reviewer"] {
        let mut snapshot = runner.state.clone();
        match stale {
            "gate" => snapshot.gate_head = Some("old".into()),
            "e2e" => snapshot.reviews.last_mut().unwrap().verified_head = Some("old".into()),
            "review" => snapshot.reviews.last_mut().unwrap().head = "old".into(),
            "missing-commands" => snapshot.gate.clear(),
            _ => snapshot.reviews.last_mut().unwrap().answered = 0,
        }
        assert!(snapshot.check_delivery_evidence(&head).is_err(), "{stale}");
    }
    let mut unapproved = runner.state.clone();
    unapproved.status = magi::run::RunStatus::Merged;
    unapproved.pr.as_mut().unwrap().state = "merged".into();
    unapproved.pr_merged_head = Some(head.clone());
    assert!(!unapproved.public_delivery_complete(), "an unanswered question is not consent");
    assert_eq!(git::remote_head(&fx.repo, "origin", &selected.branch).await.unwrap(), Some(head.clone()));
    magi::daemon::finish_attempt(3, &queue, &mut task, &runner.state, &[], result);
    assert_ne!(task.status, magi::queue::TaskStatus::Done);
    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert_eq!(resumed.state.pr_recovery, Some(selected.clone()));
    assert_eq!(resumed.state.land_approval.as_ref().unwrap().question, approval.question);
    assert!(resumed.state.parked);
    assert_eq!(std::fs::read_to_string(resumed.state.winner().unwrap().worktree.join("reviewed-fix.txt")).unwrap(), "preserved fix");
    let merges = std::fs::read_to_string(bin.join("merges")).unwrap_or_default();
    assert!(merges.lines().all(|l| l.contains("--disable-auto")), "{merges}");
    magi::ask::Questions::open().update(&approval.question, |q|
        q.answer(magi::ask::Answer::Choice("merge".into()))).unwrap();
    std::fs::write(bin.join("allow-merge"), "").unwrap();
    let result = resumed.execute().await;
    assert!(result.is_ok(), "{result:?}");
    assert!(resumed.state.public_delivery_complete(), "{:?}", resumed.state.events);
    let mut incomplete = resumed.state.clone();
    incomplete.pr_merged_head = Some("another-head".into());
    assert!(!incomplete.public_delivery_complete());
    incomplete.pr_merged_head = Some(head.clone());
    incomplete.merge.as_mut().unwrap().empty = true;
    assert!(!incomplete.public_delivery_complete(), "no-op success is not delivery");
    magi::daemon::finish_attempt(3, &queue, &mut task, &resumed.state, &[], result);
    assert_eq!(task.status, magi::queue::TaskStatus::Done);
    let merges = std::fs::read_to_string(bin.join("merges")).unwrap();
    assert!(merges.lines().filter(|l| !l.contains("--disable-auto"))
        .all(|l| l.contains("--match-head-commit") && l.contains(&head)), "{merges}");
}
}

common::e2e! {
async fn selected_review_with_no_verification_finishes_locally_without_public_delivery() {
    let (fx, mut task, mut selected, _bin) = selected_fixture().await;
    selected.disposition = Some(magi::pr_recovery::Disposition::Review);
    task.pr_recovery = Some(selected.clone());
    let mut config = fx.config.clone();
    config.verify.gate.clear();
    config.verify.e2e.clear();
    let mut runner = Runner::recover_pr(&fx.repo, &task.id, selected.clone(), config).await.unwrap();
    task.start(runner.state.id.clone());
    let queue = magi::queue::Queue::open();
    queue.put(&mut task).unwrap();
    let result = runner.execute().await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(runner.state.status, magi::run::RunStatus::Ready);
    assert_eq!(runner.state.gate_status(), magi::run::GateStatus::NotConfigured);
    assert!(!runner.state.gate_status().ok());
    assert!(runner.state.events.iter().any(|e| e.message.contains("not tested")));
    assert_eq!(git::remote_head(&fx.repo, "origin", &selected.branch).await.unwrap().as_deref(), Some(selected.head.as_str()));
    magi::daemon::finish_attempt(3, &queue, &mut task, &runner.state, &[], result);
    assert_eq!(task.status, magi::queue::TaskStatus::Done);
}
}

common::e2e! {
async fn selected_delivery_without_verification_or_disposition_stops_before_a_run_is_minted() {
    let (fx, task, mut selected, _bin) = selected_fixture().await;
    let mut config = fx.config.clone();
    config.verify.gate.clear();
    let runs = || std::fs::read_dir(magi::run::home().join("runs")).unwrap().count();
    let before = runs();
    for missing in ["gate", "e2e", "disposition"] {
        if missing == "e2e" { config.verify.gate = fx.config.verify.gate.clone(); config.verify.e2e.clear(); }
        if missing == "disposition" { selected.disposition = None; }
        let error = Runner::recover_pr(&fx.repo, &task.id, selected.clone(), config.clone()).await.err().unwrap();
        assert!(error.to_string().contains(missing), "{error:#}");
        assert_eq!(runs(), before);
        let path = fx.tmp.path().join("incomplete.toml");
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let queue = magi::queue::Queue::open();
        let mut queued = task.clone();
        queued.release_operator("test", None);
        queued.pr_recovery = Some(selected.clone());
        queued.overrides.as_mut().unwrap().config = Some(path);
        queue.put(&mut queued).unwrap();
        let opts = magi::daemon::Opts { repo: fx.repo.clone(), ..Default::default() };
        Box::pin(magi::daemon::run_claimed(&opts, &queue, &mut queued)).await;
        assert_eq!(queued.status, magi::queue::TaskStatus::Held);
        assert_eq!(queued.attempts, 0);
        assert_eq!(queued.runs, task.runs);
        assert!(queued.operator_held());
        assert!(queued.hold_reason.as_deref().unwrap().contains(missing));
        assert_eq!(runs(), before);
    }
}
}

async fn assert_recovery_resume_uses_saved_config(legacy: bool) {
    let (fx, mut task, selected, bin) = Box::pin(selected_fixture()).await;
    let queue = magi::queue::Queue::open();
    let path = fx.tmp.path().join("mutable-recovery.toml");
    std::fs::write(&path, toml::to_string(&fx.config).unwrap()).unwrap();
    task.release_operator("test", None);
    task.pr_recovery = Some(selected);
    task.overrides.as_mut().unwrap().config = Some(path.clone());
    queue.put(&mut task).unwrap();
    let opts = magi::daemon::Opts {
        repo: fx.repo.clone(),
        ..Default::default()
    };
    Box::pin(magi::daemon::run_claimed(&opts, &queue, &mut task)).await;
    assert_eq!(
        task.status,
        magi::queue::TaskStatus::Held,
        "{:?} {:?}",
        task.hold_reason,
        task.last_error
    );
    let id = task.runs.last().unwrap().clone();
    let mut state = RunState::load(&id).unwrap();
    assert!(
        state.parked,
        "hold={:?}, error={:?}, events={:?}",
        task.hold_reason, task.last_error, state.events
    );
    let approval = state.land_approval.clone().unwrap();
    let head = state.pr_pushed_head.clone().unwrap();
    let history = task.runs.clone();
    let provenance = serde_json::to_value(&state.recovery_config).unwrap();
    let gate = state.config.verify.gate.clone();
    let e2e = state.config.verify.e2e.clone();
    if legacy {
        let old = state.pr_recovery.as_mut().unwrap();
        old.disposition = None;
        old.source_head = None;
        task.pr_recovery = state.pr_recovery.clone();
        let mut unbound = state.clone();
        unbound.land_approval = None;
        assert!(unbound.configure_recovery_resume().is_err());
        let mut mismatched = state.clone();
        mismatched.pr.as_mut().unwrap().url.push_str("/different");
        assert!(mismatched.configure_recovery_resume().is_err());
        state.save().unwrap();
        queue.put(&mut task).unwrap();
        let mut resumed = Runner::resume(&id).unwrap();
        Box::pin(resumed.execute()).await.unwrap();
        assert!(resumed.state.parked);
        assert_eq!(
            resumed.state.land_approval.as_ref().unwrap().question,
            approval.question
        );
    }
    let mut invalid_saved = state.clone();
    invalid_saved.config.verify.gate.clear();
    assert!(
        invalid_saved.configure_recovery_resume().is_err(),
        "legacy records cannot waive validation"
    );
    let mut incomplete = fx.config.clone();
    incomplete.verify.gate.clear();
    incomplete.verify.e2e.clear();
    std::fs::write(&path, toml::to_string(&incomplete).unwrap()).unwrap();
    for removed in [false, true] {
        if removed {
            std::fs::remove_file(&path).unwrap();
        }
        Box::pin(magi::daemon::run_claimed(&opts, &queue, &mut task)).await;
        assert_eq!(
            task.status,
            magi::queue::TaskStatus::Held,
            "{:?}",
            task.hold_reason
        );
        assert_eq!(task.runs, history);
        let saved = RunState::load(&id).unwrap();
        assert!(saved.parked, "{:?}", saved.events);
        assert_eq!(saved.config.verify.gate, gate);
        assert_eq!(saved.config.verify.e2e, e2e);
        assert_eq!(
            saved.land_approval.as_ref().unwrap().question,
            approval.question
        );
        assert_eq!(saved.gate_head.as_deref(), Some(head.as_str()));
        assert_eq!(
            serde_json::to_value(&saved.recovery_config).unwrap(),
            provenance
        );
        assert_eq!(
            std::fs::read_to_string(saved.winner().unwrap().worktree.join("reviewed-fix.txt"))
                .unwrap(),
            "preserved fix"
        );
    }
    magi::ask::Questions::open()
        .update(&approval.question, |q| {
            q.answer(magi::ask::Answer::Choice("merge".into()))
        })
        .unwrap();
    std::fs::write(bin.join("allow-merge"), "").unwrap();
    Box::pin(magi::daemon::run_claimed(&opts, &queue, &mut task)).await;
    assert_eq!(
        task.status,
        magi::queue::TaskStatus::Done,
        "{:?}",
        task.hold_reason
    );
    assert_eq!(task.runs, history);
    let landed = RunState::load(&id).unwrap();
    assert!(landed.public_delivery_complete());
    assert_eq!(landed.pr_merged_head.as_deref(), Some(head.as_str()));
    assert_eq!(
        landed.pr_recovery.as_ref().unwrap().disposition.is_none(),
        legacy
    );
}

// Several complete graph resumes share one debug-build future in these scenes.
fn recovery_resume_scene(name: &'static str, legacy: bool) {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            common::isolated(name, || assert_recovery_resume_uses_saved_config(legacy));
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn selected_recovery_resumes_saved_validation_after_current_config_is_emptied_or_deleted() {
    recovery_resume_scene(
        "selected_recovery_resumes_saved_validation_after_current_config_is_emptied_or_deleted",
        false,
    );
}

#[test]
fn legacy_recovery_with_public_pr_and_approval_resumes_saved_delivery() {
    recovery_resume_scene(
        "legacy_recovery_with_public_pr_and_approval_resumes_saved_delivery",
        true,
    );
}

common::e2e! {
async fn rebased_retained_source_keeps_review_fixes_and_public_lease() {
    let (fx, mut task, mut selected, _bin) = selected_fixture().await;
    let queue = magi::queue::Queue::open();
    queue.put(&mut task).unwrap();
    let mut retained = RunState::load(&task.runs[0]).unwrap();
    let wt = retained.winner().unwrap().worktree.clone();
    std::fs::write(fx.repo.join("new-main.txt"), "current main must survive\n").unwrap();
    git::git(&fx.repo, &["add", "."]).await.unwrap();
    git::git(&fx.repo, &["commit", "-m", "feat: advance main"]).await.unwrap();
    let base = git::rev_parse(&fx.repo, "HEAD").await.unwrap();
    git::git(&fx.repo, &["push", "origin", "HEAD:main"]).await.unwrap();
    let old_base = git::merge_base(&fx.repo, &selected.head, &base).await.unwrap();
    git::git(&wt, &["rebase", "--onto", &base, &old_base]).await.unwrap();
    std::fs::write(wt.join("later-review.txt"), "later reviewer fix\n").unwrap();
    git::git(&wt, &["add", "."]).await.unwrap();
    git::git(&wt, &["commit", "-m", "fix: retain later review changes"]).await.unwrap();
    let source = git::rev_parse(&wt, "HEAD").await.unwrap();
    assert!(!git::is_ancestor(&fx.repo, &selected.head, &source).await);
    let mut original = selected.clone();
    original.source_head = None;
    original.disposition = None;
    retained.pr_recovery = Some(original);
    retained.origin = Some(Origin::queue(&task.id));
    retained.base_sync = Some(magi::run::BaseSync {
        tip: base.clone(), behind: 0, attempts: 1, conflict: None, already_in: None,
    });
    retained.reviews.last_mut().unwrap().head = source.clone();
    let branch = retained.winner().unwrap().branch.clone();
    retained.event("land", format!("rebased {branch} onto origin/main"));
    retained.save().unwrap();
    selected.source_head = Some(source.clone());
    selected.source_run = Some(retained.id.clone());
    // Main can advance again after the retained review; the new run must sync and review anew.
    std::fs::write(fx.repo.join("latest-main.txt"), "latest main must survive\n").unwrap();
    git::git(&fx.repo, &["add", "."]).await.unwrap();
    git::git(&fx.repo, &["commit", "-m", "feat: advance main after retained review"]).await.unwrap();
    git::git(&fx.repo, &["push", "origin", "HEAD:main"]).await.unwrap();
    let latest = git::rev_parse(&fx.repo, "HEAD").await.unwrap();
    let config_path = fx.tmp.path().join("recovery.toml");
    std::fs::write(&config_path, toml::to_string(&fx.config).unwrap()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_magi"))
        .args(["task", "recover-pr", &task.id, "--pr", &selected.url,
            "--head", &selected.head, "--source-head", &source,
            "--source-run", &retained.id, "--disposition", "land", "--config"])
        .arg(&config_path).env("MAGI_HOME", magi::run::home()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    task = queue.get(&task.id).unwrap();
    selected = task.pr_recovery.clone().unwrap();
    let proof = selected.source_proof.clone().unwrap();
    assert_eq!(proof.new_base, base);
    assert_eq!(proof.current_base, latest);
    let mut runner = Runner::recover_pr(&fx.repo, &task.id, selected.clone(), fx.config.clone()).await.unwrap();
    task.pr_recovery = Some(selected.clone());
    task.runs.push(runner.state.id.clone());
    queue.put(&mut task).unwrap();
    runner.execute().await.unwrap();
    let recovered = &runner.state.winner().unwrap().worktree;
    for (file, expected) in [("new-main.txt", "current main must survive\n"),
        ("reviewed-fix.txt", "preserved fix"), ("later-review.txt", "later reviewer fix\n"),
        ("latest-main.txt", "latest main must survive\n")] {
        assert_eq!(std::fs::read_to_string(recovered.join(file)).unwrap(), expected);
    }
    assert_eq!(runner.state.pr_recovery.as_ref().unwrap().source_proof.as_ref(), Some(&proof));
    assert!(runner.state.parked);
    let final_head = git::rev_parse(recovered, "HEAD").await.unwrap();
    assert_ne!(final_head, source);
    assert!(git::is_ancestor(&fx.repo, &latest, &final_head).await);
    assert_eq!(runner.state.land_approval.as_ref().unwrap().head, final_head);
    runner.state.check_delivery_evidence(&final_head).unwrap();
    assert!(runner.state.check_delivery_evidence(&source).is_err());
    // The original public lease cannot authorize another recovery after publication.
    let error = selected.verify(&fx.repo, "origin", "main", &selected.head).await.unwrap_err().to_string();
    assert!(error.contains("moved"), "{error}");
    // Resuming must reproduce the saved proof, even after publication changed the public head.
    let mut resumed = Runner::resume(&runner.state.id).unwrap();
    resumed.execute().await.unwrap();
    assert!(resumed.state.parked, "{:?}: {:?}", resumed.state.status, resumed.state.events);
    assert_eq!(resumed.state.pr_recovery, Some(selected));
    resumed.state.pr_recovery.as_mut().unwrap().source_proof.as_mut().unwrap().commits[0].2 = "0".repeat(40);
    assert!(resumed.execute().await.unwrap_err().to_string().contains("proof changed"));
    assert_eq!(git::remote_head(&fx.repo, "origin", "magi/task-abcd-1234").await.unwrap(), Some(final_head));
}
}
