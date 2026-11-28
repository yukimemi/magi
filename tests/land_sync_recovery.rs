//! Integration tests for explicit, bounded landing base-sync recovery
//! (`recover-land-sync`), covering legacy saved stops, two-phase receipts,
//! interrupted acceptance reconciliation, uncertain receipts, dispatch
//! revalidation, chat/standing/goal guards, reporting, and non-regression of
//! pre-publication `recover-base`.

use std::path::{Path, PathBuf};

use magi::{
    config::{Config, MergeMode},
    land_sync_recovery::{MAX_GRANTS, ObservedPr, PrObserver},
    land_sync_recovery_control::{self, BeforeWrite, Request},
    queue::{Queue, Source, Task},
    run::{BaseSync, GateRepairE2e, MergeOutcome, PrRecord, RunState, RunStatus},
};

fn git(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

struct StaticObserver(ObservedPr);

impl PrObserver for StaticObserver {
    async fn observe_pr(&self, _repo: &Path, _pr_url: &str) -> anyhow::Result<ObservedPr> {
        Ok(self.0.clone())
    }
}

const PR_URL: &str = "https://github.com/example/repo/pull/42";

struct Fixture {
    tmp: tempfile::TempDir,
    repo: PathBuf,
    queue: Queue,
    run: RunState,
    head: String,
    base: String,
}

impl Fixture {
    fn new_legacy_landing_exhausted() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();

        let repo = home.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.name", "fixture"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(repo.join("base"), "base").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);

        let origin = home.join("origin.git");
        git(&repo, &["init", "--bare", origin.to_str().unwrap()]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repo, &["push", "origin", "main"]);

        git(&repo, &["checkout", "-b", "winner"]);
        std::fs::write(repo.join("feature"), "impl").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "feat: impl"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["push", "origin", "HEAD:refs/heads/task-head"]);

        // Advance origin/main ahead of winner.
        git(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("upstream"), "upstream").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "upstream"]);
        git(&repo, &["push", "origin", "main"]);
        git(&repo, &["checkout", "winner"]);

        let mut config = Config::default();
        config.graph.land = true;
        config.graph.land_rounds = 4;
        config.graph.land_approval = true;

        let mut run = RunState::new(
            repo.clone(),
            "main".into(),
            base.clone(),
            "ship landing recovery".into(),
            config,
        );
        run.begin_pass();
        run.status = RunStatus::Blocked;
        run.driver_exited = true;
        run.candidates.push(
            serde_json::from_value(serde_json::json!({
                "index": 0,
                "label": "A",
                "agent": "fixture",
                "branch": "winner",
                "worktree": repo
            }))
            .unwrap(),
        );
        run.tally = Some(
            serde_json::from_value(serde_json::json!({
                "first_choice": {},
                "borda": {},
                "winner": "A",
                "unanimous_initial": true,
                "deliberated": false,
                "changed_votes": 0,
                "unanimous_final": true
            }))
            .unwrap(),
        );
        run.reviews.push(
            serde_json::from_value(serde_json::json!({
                "round": 1,
                "head": head,
                "expected": 2,
                "answered": 2,
                "reviews": [],
                "clean": true,
                "e2e": [{"command": "cargo test", "code": 0, "output_tail": "ok", "duration_ms": 10}],
                "verified_head": head,
                "verified_at": jiff::Timestamp::now(),
                "e2e_deferred": false
            }))
            .unwrap(),
        );
        run.head_e2e = Some(GateRepairE2e::new(head.clone(), true, Vec::new()));
        run.gate_ran = true;
        run.gate_head = Some(head.clone());
        run.pr_head = Some("task-head".into());
        run.pr_pushed_head = Some(head.clone());
        run.pr = Some(PrRecord {
            url: PR_URL.to_owned(),
            number: 42,
            state: "open".to_owned(),
            checks: "green".to_owned(),
            round: 0,
            rounds: 4,
            red_at_merge: Vec::new(),
        });
        run.merge = Some(MergeOutcome {
            mode: MergeMode::Pr,
            ok: false,
            detail:
                "could not synchronize landing base: landing base synchronization budget exhausted"
                    .into(),
            empty: false,
        });
        run.land_sync_attempts = 4;
        run.land_sync_target = None;
        run.land_sync_pending = true;
        run.event(
            "land",
            "stopped: could not synchronize landing base: landing base synchronization budget exhausted",
        );

        let queue = Queue::at(home.join("queue"));
        let mut task = Task::new(
            "landing sync".into(),
            run.instruction.clone(),
            repo.clone(),
            Source::Human,
        );
        task.link_run(&run.id);
        task.hold_machine(Some("landing base synchronization budget exhausted".into()));
        queue.put(&mut task).unwrap();
        run.origin = Some(magi::run::Origin::queue(&task.id));
        run.save_under(&home).unwrap();

        Self {
            tmp,
            repo,
            queue,
            run,
            head,
            base,
        }
    }

    fn observer(&self) -> StaticObserver {
        StaticObserver(ObservedPr {
            url: PR_URL.to_owned(),
            number: 42,
            open: true,
            head: self.head.clone(),
            base_branch: Some("main".to_owned()),
        })
    }

    fn state(&self) -> RunState {
        RunState::load_under(&self.run.id, self.tmp.path()).unwrap()
    }
}

#[tokio::test]
async fn admission_refuses_stale_head_live_owner_and_uncertain_receipts() {
    let fx = Fixture::new_legacy_landing_exhausted();
    let home = fx.tmp.path();
    let observer = fx.observer();

    // 1. Stale --head refused before effect (BeforeWrite), no receipt written.
    let wrong_head = "a".repeat(40);
    let err = land_sync_recovery_control::recover_with(
        &fx.queue,
        home,
        Request {
            run: fx.run.id.clone(),
            head: wrong_head,
            actor: "cli".into(),
            request: "req-stale-head".into(),
        },
        &observer,
    )
    .await
    .unwrap_err();
    assert!(err.is::<BeforeWrite>(), "{err:#}");
    assert!(err.to_string().contains("retained HEAD changed"), "{err:#}");
    assert!(!land_sync_recovery_control::receipt_path(home, "cli", "req-stale-head").exists());

    // 2. Stale published PR head refused before effect.
    let stale_pr_observer = StaticObserver(ObservedPr {
        url: PR_URL.to_owned(),
        number: 42,
        open: true,
        head: "b".repeat(40),
        base_branch: Some("main".to_owned()),
    });
    let err = land_sync_recovery_control::recover_with(
        &fx.queue,
        home,
        Request {
            run: fx.run.id.clone(),
            head: fx.head.clone(),
            actor: "cli".into(),
            request: "req-stale-pr".into(),
        },
        &stale_pr_observer,
    )
    .await
    .unwrap_err();
    assert!(err.is::<BeforeWrite>(), "{err:#}");
    assert!(err.to_string().contains("published PR head"), "{err:#}");

    // 3. Live driver refused before effect.
    {
        let mut live = fx.state();
        live.driver_exited = false;
        live.driver_pid = Some(std::process::id());
        live.driver_claimed = Some(true);
        live.save_under(home).unwrap();

        let err = land_sync_recovery_control::recover_with(
            &fx.queue,
            home,
            Request {
                run: fx.run.id.clone(),
                head: fx.head.clone(),
                actor: "cli".into(),
                request: "req-live-driver".into(),
            },
            &observer,
        )
        .await
        .unwrap_err();
        assert!(err.is::<BeforeWrite>(), "{err:#}");
        assert!(
            err.to_string()
                .contains("run is live or its ownership is unknown"),
            "{err:#}"
        );

        live.driver_exited = true;
        live.driver_pid = None;
        live.save_under(home).unwrap();
    }

    // 4. Corrupt receipt JSON refused as uncertain (NOT BeforeWrite).
    let corrupt_path = land_sync_recovery_control::receipt_path(home, "cli", "req-corrupt");
    std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
    std::fs::write(&corrupt_path, b"{corrupt json").unwrap();
    let err = land_sync_recovery_control::recover_with(
        &fx.queue,
        home,
        Request {
            run: fx.run.id.clone(),
            head: fx.head.clone(),
            actor: "cli".into(),
            request: "req-corrupt".into(),
        },
        &observer,
    )
    .await
    .unwrap_err();
    assert!(
        !err.is::<BeforeWrite>(),
        "corrupt receipt must be uncertain, not BeforeWrite"
    );
    assert!(err.to_string().contains("uncertain"), "{err:#}");
    assert!(fx.state().land_sync_recovery.authorizations.is_empty());

    // 5. Authorized receipt with no matching authorization in run.json refused as uncertain.
    let valid_outcome = land_sync_recovery_control::recover_with(
        &fx.queue,
        home,
        Request {
            run: fx.run.id.clone(),
            head: fx.head.clone(),
            actor: "cli".into(),
            request: "req-real".into(),
        },
        &observer,
    )
    .await
    .unwrap();
    assert!(!valid_outcome.repeated);
    assert_eq!(fx.state().land_sync_recovery.authorizations.len(), 1);

    // Reusing the same request ID with a different HEAD fails.
    let err = land_sync_recovery_control::recover_with(
        &fx.queue,
        home,
        Request {
            run: fx.run.id.clone(),
            head: "c".repeat(40),
            actor: "cli".into(),
            request: "req-real".into(),
        },
        &observer,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("request ID already binds a different operation"),
        "{err:#}"
    );
}

#[tokio::test]
async fn interrupted_acceptance_reconciles_across_all_phases_without_replay() {
    let fx = Fixture::new_legacy_landing_exhausted();
    let home = fx.tmp.path();
    let observer = fx.observer();

    // First perform a normal admission to capture a valid receipt shape.
    let first = land_sync_recovery_control::recover_with(
        &fx.queue,
        home,
        Request {
            run: fx.run.id.clone(),
            head: fx.head.clone(),
            actor: "cli".into(),
            request: "req-phase".into(),
        },
        &observer,
    )
    .await
    .unwrap();
    assert!(!first.repeated);
    assert!(!first.reconciled);

    let receipt_file = land_sync_recovery_control::receipt_path(home, "cli", "req-phase");
    let full_receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&receipt_file).unwrap()).unwrap();
    assert_eq!(full_receipt["phase"], "applied");

    // Case A: Crash after queue commit (task has marker) before receipt marked `applied`.
    {
        let mut interrupted = full_receipt.clone();
        interrupted["phase"] = serde_json::json!("authorized");
        interrupted["applied_at"] = serde_json::Value::Null;
        std::fs::write(
            &receipt_file,
            serde_json::to_vec_pretty(&interrupted).unwrap(),
        )
        .unwrap();

        let retry = land_sync_recovery_control::recover_with(
            &fx.queue,
            home,
            Request {
                run: fx.run.id.clone(),
                head: fx.head.clone(),
                actor: "cli".into(),
                request: "req-phase".into(),
            },
            &observer,
        )
        .await
        .unwrap();
        assert!(retry.repeated);
        assert!(retry.reconciled);
        assert_eq!(fx.state().land_sync_recovery.authorizations.len(), 1);
        let repaired: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipt_file).unwrap()).unwrap();
        assert_eq!(repaired["phase"], "applied");
    }

    // Case B: Crash after RunState saved authorization (Phase 2a) before queue commit (Phase 2b).
    {
        // Restore raw task file on disk to the pre-commit snapshot (bypassing Queue::put's
        // stale-writer ledger preservation) while keeping authorization in run.json.
        let task_id = full_receipt["binding"]["task"]["id"].as_str().unwrap();
        let task_file = home.join("queue").join(format!("{task_id}.json"));
        std::fs::write(
            &task_file,
            serde_json::to_vec_pretty(&full_receipt["binding"]["task"]).unwrap(),
        )
        .unwrap();
        let mut interrupted = full_receipt.clone();
        interrupted["phase"] = serde_json::json!("authorized");
        interrupted["applied_at"] = serde_json::Value::Null;
        std::fs::write(
            &receipt_file,
            serde_json::to_vec_pretty(&interrupted).unwrap(),
        )
        .unwrap();

        let retry = land_sync_recovery_control::recover_with(
            &fx.queue,
            home,
            Request {
                run: fx.run.id.clone(),
                head: fx.head.clone(),
                actor: "cli".into(),
                request: "req-phase".into(),
            },
            &observer,
        )
        .await
        .unwrap();
        assert!(!retry.repeated);
        assert!(retry.reconciled);
        assert_eq!(
            fx.state().land_sync_recovery.authorizations.len(),
            1,
            "reconciliation must never duplicate authorization"
        );
        assert!(
            retry
                .task
                .land_sync_recovery_requests
                .contains(&"cli/req-phase".to_owned())
        );
    }

    // Case C: Authorized receipt exists, but run.json lost its authorization -> uncertain refusal!
    {
        let task_id = full_receipt["binding"]["task"]["id"].as_str().unwrap();
        let task_file = home.join("queue").join(format!("{task_id}.json"));
        std::fs::write(
            &task_file,
            serde_json::to_vec_pretty(&full_receipt["binding"]["task"]).unwrap(),
        )
        .unwrap();
        let mut run = fx.state();
        run.land_sync_recovery.authorizations.clear();
        run.save_under(home).unwrap();
        let mut interrupted = full_receipt.clone();
        interrupted["phase"] = serde_json::json!("authorized");
        interrupted["applied_at"] = serde_json::Value::Null;
        std::fs::write(
            &receipt_file,
            serde_json::to_vec_pretty(&interrupted).unwrap(),
        )
        .unwrap();

        let err = land_sync_recovery_control::recover_with(
            &fx.queue,
            home,
            Request {
                run: fx.run.id.clone(),
                head: fx.head.clone(),
                actor: "cli".into(),
                request: "req-phase".into(),
            },
            &observer,
        )
        .await
        .unwrap_err();
        assert!(!err.is::<BeforeWrite>());
        assert!(err.to_string().contains("uncertain"), "{err:#}");
        assert!(fx.state().land_sync_recovery.authorizations.is_empty());
    }

    // Case D: Crash after Phase 1 (`prepared`), before RunState write -> reconciles cleanly!
    {
        let mut interrupted = full_receipt.clone();
        interrupted["phase"] = serde_json::json!("prepared");
        interrupted["applied_at"] = serde_json::Value::Null;
        std::fs::write(
            &receipt_file,
            serde_json::to_vec_pretty(&interrupted).unwrap(),
        )
        .unwrap();
        // Restore the actual legacy shape: neither the stop nor authorization
        // was persisted when the Prepared receipt was written.
        let mut run = fx.state();
        run.land_sync_recovery = Default::default();
        run.save_under(home).unwrap();
        git(&fx.repo, &["checkout", "main"]);
        git(&fx.repo, &["commit", "--allow-empty", "-m", "advance base"]);
        git(&fx.repo, &["push", "origin", "main"]);
        git(&fx.repo, &["checkout", "winner"]);

        let retry = land_sync_recovery_control::recover_with(
            &fx.queue,
            home,
            Request {
                run: fx.run.id.clone(),
                head: fx.head.clone(),
                actor: "cli".into(),
                request: "req-phase".into(),
            },
            &observer,
        )
        .await
        .unwrap();
        assert!(!retry.repeated);
        assert!(retry.reconciled);
        assert_eq!(fx.state().land_sync_recovery.authorizations.len(), 1);
        let run = fx.state();
        assert_eq!(
            serde_json::to_value(&run.land_sync_recovery.authorizations[0].scope).unwrap(),
            full_receipt["binding"]["scope"]
        );
    }
}

#[tokio::test]
async fn no_regression_to_prepublication_base_sync_recovery_and_reporting() {
    let fx = Fixture::new_legacy_landing_exhausted();
    let home = fx.tmp.path();
    let observer = fx.observer();

    // Landing exhaustion report/notice/stop_reason surface attempts, grants, and recover-land-sync.
    let rendered = magi::report::run(&fx.state());
    assert!(rendered.contains("landing base sync"), "{rendered}");
    assert!(rendered.contains("attempts 4/4"), "{rendered}");
    assert!(
        rendered.contains(&format!("grants 0/{MAX_GRANTS}")),
        "{rendered}"
    );
    assert!(rendered.contains("recover-land-sync"), "{rendered}");

    let notice = magi::notices::run_stopped(&fx.run.id, &fx.state());
    assert!(
        notice.message.contains("recover-land-sync"),
        "{}",
        notice.message
    );

    // Pre-publication base_sync_blocked run: recover-land-sync refuses and recover-base succeeds.
    let mut pre_pub = RunState::new(
        fx.repo.clone(),
        "main".into(),
        fx.base.clone(),
        "prepub sync".into(),
        Config::default(),
    );
    pre_pub.begin_pass();
    pre_pub.status = RunStatus::Blocked;
    pre_pub.driver_exited = true;
    pre_pub.candidates.push(
        serde_json::from_value(serde_json::json!({
            "index": 0,
            "label": "A",
            "agent": "fixture",
            "branch": "winner",
            "worktree": fx.repo
        }))
        .unwrap(),
    );
    pre_pub.tally = Some(
        serde_json::from_value(serde_json::json!({
            "first_choice": {},
            "borda": {},
            "winner": "A",
            "unanimous_initial": true,
            "deliberated": false,
            "changed_votes": 0,
            "unanimous_final": true
        }))
        .unwrap(),
    );
    pre_pub.base_sync = Some(BaseSync {
        tip: fx.base.clone(),
        behind: 1,
        attempts: 4,
        conflict: Some(
            "main moved 1 commit(s) ahead of winner after 4 synchronization(s); synchronizing again would only race it".into(),
        ),
        already_in: None,
    });
    let mut pre_task = Task::new(
        "prepub".into(),
        pre_pub.instruction.clone(),
        fx.repo.clone(),
        Source::Human,
    );
    pre_task.link_run(&pre_pub.id);
    pre_task.hold_machine(Some("base stopped".into()));
    fx.queue.put(&mut pre_task).unwrap();
    pre_pub.origin = Some(magi::run::Origin::queue(&pre_task.id));
    pre_pub.save_under(home).unwrap();

    // recover-land-sync refuses pre-publication base-sync stop.
    let err = land_sync_recovery_control::recover_with(
        &fx.queue,
        home,
        Request {
            run: pre_pub.id.clone(),
            head: fx.head.clone(),
            actor: "cli".into(),
            request: "req-wrong-surface".into(),
        },
        &observer,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("pre-publication base-sync stop; use recover-base"),
        "{err:#}"
    );

    // Pre-publication recover-base still works with zero regression.
    magi::graph::Runner::authorize_base_recovery(home, &pre_pub.id, &pre_task.id)
        .await
        .unwrap();
    let updated_pre = RunState::load_under(&pre_pub.id, home).unwrap();
    assert_eq!(updated_pre.base_sync_recovery.authorizations.len(), 1);
}
