//! End-to-end: the base moves while a run is still working.
//!
//! `verify.e2e`, `verify.gate` and every reviewer read whatever is checked out
//! in the winner's worktree. Left alone that stays rooted at the commit the
//! run branched from, and a run takes long enough that the base usually moves
//! before it gets to the gate - a green run whose merge would revert whatever
//! landed elsewhere while it was thinking. `Runner::sync_to_base` is what
//! rebases the winner onto the current tip of `<remote>/<base>` before
//! anything verifies it; these tests are the two ways that can go.
mod common;

use common::{Judges, fixture, home_lock};
use magi::graph::Runner;
use magi::run::RunStatus;

fn run_git(repo: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn is_ancestor(repo: &std::path::Path, ancestor: &str, of: &str) -> bool {
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, of])
        .current_dir(repo)
        .status()
        .expect("spawn git")
        .success()
}

/// A bare `origin` for `fx.repo`, plus a second clone that can push to it
/// independently of anything `fx.repo` or its worktrees are doing -
/// simulating another pull request landing while a run is still working.
struct Origin {
    sideline: std::path::PathBuf,
}

fn wire_origin(fx: &common::Fixture) -> Origin {
    let origin = fx.tmp.path().join("origin.git");
    run_git(
        fx.tmp.path(),
        &[
            "clone",
            "--bare",
            "-q",
            fx.repo.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    run_git(
        &fx.repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );

    let sideline = fx.tmp.path().join("sideline");
    run_git(
        fx.tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            sideline.to_str().unwrap(),
        ],
    );
    run_git(&sideline, &["config", "user.name", "other pr"]);
    run_git(&sideline, &["config", "user.email", "other@example.com"]);
    Origin { sideline }
}

/// Land one more commit on `origin`'s `main`, from the sideline clone.
fn land_on_origin(sideline: &std::path::Path, file: &str, content: &str) {
    std::fs::write(sideline.join(file), content).unwrap();
    run_git(sideline, &["add", "-A"]);
    run_git(sideline, &["commit", "-q", "-m", "another PR landed"]);
    run_git(sideline, &["push", "-q", "origin", "main"]);
}

common::e2e! {
async fn the_gate_runs_on_a_tree_that_contains_what_landed_while_the_run_was_thinking() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    // Base-sync behaviour does not depend on the panel: a solo candidate
    // skips judging and reaches the same winner-under-rebase scenario for a
    // fraction of the subprocess cost.
    fx.config.graph.candidates = 1;
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");

    // The run has branched (`base_commit` is already fixed); now, while it
    // works, somebody else's pull request lands on the base.
    land_on_origin(
        &origin.sideline,
        "upstream.txt",
        "landed while the run was thinking\n",
    );

    runner.execute().await.expect("execute");
    let state = &runner.state;

    let sync = state.base_sync.as_ref().expect("base sync recorded");
    assert!(
        sync.conflict.is_none(),
        "unexpected conflict: {:?}",
        sync.conflict
    );
    assert_eq!(sync.behind, 0, "resynced before the gate ran");
    assert!(sync.attempts >= 1, "a rebase must have happened");

    let winner = state.winner().expect("a winner");
    assert!(
        winner.worktree.join("upstream.txt").is_file(),
        "the tree the gate ran on must contain what landed on the base"
    );
    assert!(
        winner.worktree.join("note.txt").is_file(),
        "and the candidate's own work must survive the rebase"
    );

    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);
    assert_eq!(state.status, RunStatus::Ready);

    // The `merge = "none"` guidance names `winner.branch`; proving it now
    // descends from the landing base's tip is what makes that guidance safe -
    // merging it will not delete `upstream.txt`.
    let tip = std::process::Command::new("git")
        .args(["rev-parse", "origin/main"])
        .current_dir(&fx.repo)
        .output()
        .expect("rev-parse");
    let tip = String::from_utf8_lossy(&tip.stdout).trim().to_owned();
    assert!(
        is_ancestor(&fx.repo, &tip, &winner.branch),
        "the winner branch must descend from the current landing base"
    );
}
}

/// Make every mock agent behave as `var` says (see the mock's rebase branch).
fn set_agent_env(fx: &mut common::Fixture, var: &str) {
    for a in &mut fx.config.agents {
        a.env.insert(var.to_owned(), "1".to_owned());
    }
}

fn rebase_fix_calls(state: &magi::run::RunState) -> usize {
    std::fs::read_to_string(state.dir().join("artifacts").join("rebase-fix.log"))
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

common::e2e! {
async fn a_conflicting_base_is_resolved_by_the_fixer_and_the_run_goes_on_to_review() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    // The base gains its own, different `note.txt` - the path the candidate
    // creates - so replaying the candidate's commit cannot avoid a conflict.
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    land_on_origin(
        &origin.sideline,
        "note.txt",
        "a conflicting note from upstream\n",
    );

    runner.execute().await.expect("execute");
    let state = &runner.state;

    let sync = state.base_sync.as_ref().expect("base sync recorded");
    assert!(sync.conflict.is_none(), "unexpected: {:?}", sync.conflict);
    assert_eq!(state.status, RunStatus::Ready);
    assert_eq!(state.rebase_fixes.len(), 1, "{:?}", state.rebase_fixes);
    assert_eq!(state.rebase_fixes[0].paths, vec!["note.txt".to_owned()]);
    assert!(state.rebase_fixes[0].finished);
    assert_eq!(rebase_fix_calls(state), 1);

    // Review and the gate ran on the resolved tree.
    assert!(!state.reviews.is_empty(), "review must have run");
    assert!(state.gate.iter().all(|o| o.ok()), "{:?}", state.gate);

    let winner = state.winner().expect("a winner");
    let note = std::fs::read_to_string(winner.worktree.join("note.txt")).unwrap();
    assert!(
        note.contains("a conflicting note from upstream"),
        "the base's side must survive: {note}"
    );
    assert!(!note.contains("<<<<<<<"), "{note}");
    let tip = std::process::Command::new("git")
        .args(["rev-parse", "origin/main"])
        .current_dir(&fx.repo)
        .output()
        .expect("rev-parse");
    let tip = String::from_utf8_lossy(&tip.stdout).trim().to_owned();
    assert!(
        is_ancestor(&fx.repo, &tip, &winner.branch),
        "the resolved branch must descend from the base"
    );
    assert!(
        !state.dir().join("base-sync").exists(),
        "the throwaway worktree is removed"
    );
}
}

common::e2e! {
async fn a_fixer_that_never_resolves_the_conflict_blocks_the_run_and_says_what_was_tried() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    set_agent_env(&mut fx, "MOCK_REBASE_FIX_NOOP");
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    let branch = runner.state.branch_for('A');
    land_on_origin(
        &origin.sideline,
        "note.txt",
        "a conflicting note from upstream\n",
    );

    runner.execute().await.expect("execute");
    let state = &runner.state;

    let sync = state.base_sync.as_ref().expect("base sync recorded");
    let why = sync.conflict.as_ref().expect("a conflict must be recorded");
    assert!(why.to_lowercase().contains("conflict"), "{why}");
    assert!(why.contains("3 of 3"), "rounds spent are named: {why}");
    assert!(why.contains("note.txt"), "remaining paths are named: {why}");
    assert_eq!(state.status, RunStatus::Blocked);
    assert!(sync.behind > 0, "the lag was recorded before the attempt");
    assert_eq!(state.rebase_fixes.len(), 3);
    assert_eq!(rebase_fix_calls(state), 3);

    // Nothing past the base sync ran.
    assert!(state.reviews.is_empty(), "{:?}", state.reviews);
    assert!(state.gate.is_empty());

    // The branch is exactly where it was and the scratch tree is gone.
    let winner = state.winner().expect("a winner");
    assert!(winner.worktree.exists(), "the worktree is kept");
    assert!(!state.dir().join("base-sync").exists());
    assert!(
        !is_ancestor(&fx.repo, "origin/main", &branch),
        "the branch must not have moved"
    );
}
}

common::e2e! {
async fn a_fixer_that_abandons_the_rebase_is_not_mistaken_for_success() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    set_agent_env(&mut fx, "MOCK_REBASE_FIX_ABORT");
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    let branch = runner.state.branch_for('A');
    land_on_origin(&origin.sideline, "note.txt", "upstream\n");

    runner.execute().await.expect("execute");
    let state = &runner.state;
    assert_eq!(state.status, RunStatus::Blocked);
    let why = state
        .base_sync
        .as_ref()
        .and_then(|s| s.conflict.clone())
        .expect("a reason is recorded");
    assert!(why.contains("without the base"), "{why}");
    assert!(state.reviews.is_empty());
    assert!(!is_ancestor(&fx.repo, "origin/main", &branch));
}
}

common::e2e! {
async fn a_fixer_that_skips_every_commit_does_not_lose_the_branch() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    set_agent_env(&mut fx, "MOCK_REBASE_FIX_SKIP");
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    land_on_origin(&origin.sideline, "note.txt", "upstream\n");

    runner.execute().await.expect("execute");
    let state = &runner.state;
    assert_eq!(state.status, RunStatus::Blocked);
    let why = state
        .base_sync
        .as_ref()
        .and_then(|s| s.conflict.clone())
        .expect("a reason is recorded");
    assert!(why.contains("all skipped"), "{why}");
    let winner = state.winner().expect("a winner");
    assert!(
        !is_ancestor(&fx.repo, "origin/main", &winner.branch),
        "the branch keeps its own commits"
    );
}
}

common::e2e! {
async fn the_conflict_round_bound_is_counted_in_state_and_survives_a_resume() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    set_agent_env(&mut fx, "MOCK_REBASE_FIX_NOOP");
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    land_on_origin(&origin.sideline, "note.txt", "upstream\n");

    // Two rounds were already spent by an earlier process of this run.
    for _ in 0..2 {
        runner.state.rebase_fixes.push(magi::run::RebaseFixRecord {
            agent: "mock".to_owned(),
            paths: vec!["note.txt".to_owned()],
            from: None,
            finished: false,
            error: None,
        });
    }
    runner.state.save().expect("save");

    runner.execute().await.expect("execute");
    assert_eq!(
        rebase_fix_calls(&runner.state),
        1,
        "only the one round left may be spent"
    );
    assert_eq!(runner.state.rebase_fixes.len(), 3);
    assert_eq!(runner.state.status, RunStatus::Blocked);

    // What was counted is what is on disk.
    let loaded = magi::run::RunState::load(&runner.state.id).expect("load");
    assert_eq!(loaded.rebase_fixes.len(), 3);
}
}

common::e2e! {
async fn with_no_conflict_rounds_a_conflict_stops_without_asking_a_fixer() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    fx.config.graph.review_rounds = 0;
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    land_on_origin(&origin.sideline, "note.txt", "upstream\n");

    runner.execute().await.expect("execute");
    let state = &runner.state;
    assert_eq!(state.status, RunStatus::Blocked);
    let why = state
        .base_sync
        .as_ref()
        .and_then(|s| s.conflict.clone())
        .expect("a reason is recorded");
    assert!(why.contains("0 of 0"), "{why}");
    assert_eq!(rebase_fix_calls(state), 0);
    assert!(state.rebase_fixes.is_empty());
}
}

common::e2e! {
async fn a_rebased_winner_is_pushed_over_the_remote_copy_magi_last_saw() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");

    // An earlier attempt published the branch; now the base moves.
    let branch = runner.state.branch_for('A');
    let base = runner.state.base_commit.clone();
    run_git(
        &fx.repo,
        &["push", "-q", "origin", &format!("{base}:refs/heads/{branch}")],
    );
    land_on_origin(&origin.sideline, "upstream.txt", "landed meanwhile\n");

    runner.execute().await.expect("execute");
    let winner = runner.state.winner().expect("a winner");
    assert!(
        runner.state.base_sync.as_ref().is_some_and(|s| s.conflict.is_none()),
        "{:?}",
        runner.state.base_sync
    );

    let rev = |repo: &std::path::Path, r: &str| {
        let out = std::process::Command::new("git")
            .args(["rev-parse", r])
            .current_dir(repo)
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    let bare = fx.tmp.path().join("origin.git");
    assert_eq!(
        rev(&bare, &format!("refs/heads/{}", winner.branch)),
        rev(&fx.repo, &format!("refs/heads/{}", winner.branch)),
        "the rebased tip must reach the remote, not stay local"
    );
}
}

common::e2e! {
async fn a_remote_branch_with_someone_elses_commits_blocks_the_base_sync() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");

    // A person pushed their own commit to the run's branch.
    let branch = runner.state.branch_for('A');
    run_git(&origin.sideline, &["checkout", "-q", "-b", "theirs"]);
    std::fs::write(origin.sideline.join("theirs.txt"), "x\n").unwrap();
    run_git(&origin.sideline, &["add", "-A"]);
    run_git(&origin.sideline, &["commit", "-q", "-m", "a person's commit"]);
    run_git(
        &origin.sideline,
        &["push", "-q", "origin", &format!("theirs:refs/heads/{branch}")],
    );
    land_on_origin(&origin.sideline, "upstream.txt", "landed meanwhile\n");
    // land_on_origin commits on the sideline's current branch; put it on main.
    run_git(&origin.sideline, &["push", "-q", "origin", "HEAD:refs/heads/main"]);

    runner.execute().await.expect("execute");
    assert_eq!(runner.state.status, RunStatus::Blocked);
    let why = runner
        .state
        .base_sync
        .as_ref()
        .and_then(|s| s.conflict.clone())
        .expect("a reason is recorded");
    assert!(why.contains("does not contain"), "{why}");
}
}

common::e2e! {
async fn a_change_that_already_landed_under_another_commit_ends_the_run_without_a_conflict() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    let origin = wire_origin(&fx);

    let mut runner = Runner::start(&fx.repo, "create note.txt".to_owned(), fx.config.clone(), magi::run::Origin::operator())
        .await
        .expect("start");
    // Another task lands exactly what the candidate is about to write - a
    // different commit, the same change.
    land_on_origin(&origin.sideline, "note.txt", "content from impl-A\n");

    runner.execute().await.expect("execute");
    let state = &runner.state;

    assert_eq!(state.status, RunStatus::AlreadyInBase, "{:?}", state.events);
    let sync = state.base_sync.as_ref().expect("base sync recorded");
    assert!(sync.conflict.is_none(), "{:?}", sync.conflict);
    let evidence = sync.already_in.as_ref().expect("evidence recorded");
    assert!(!evidence.tip.is_empty());
    assert!(state.reviews.is_empty(), "nothing left to review");
    assert!(state.merge.is_none(), "this run merged nothing");
    assert!(
        state.events.iter().any(|e| e.message.contains("is already in")),
        "{:?}",
        state.events
    );
    assert!(!state.status.resumable());
}
}

fn rev(repo: &std::path::Path, r: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", r])
        .current_dir(repo)
        .output()
        .expect("rev-parse");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Run to the end with the base moved and no remote copy of the branch, so the
/// winner is rebased locally and nothing is pushed. Returns the runner, the
/// branch, and a commit that carries the winner's patch on the *old* base
/// (what an earlier published attempt of the branch would have looked like).
async fn rebased_locally_only(fx: &common::Fixture, origin: &Origin) -> (Runner, String, String) {
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".to_owned(),
        fx.config.clone(),
        magi::run::Origin::operator(),
    )
    .await
    .expect("start");
    let base = runner.state.base_commit.clone();
    land_on_origin(&origin.sideline, "upstream.txt", "landed meanwhile\n");
    runner.execute().await.expect("execute");
    let branch = runner.state.winner().expect("a winner").branch.clone();
    assert!(
        runner
            .state
            .base_sync
            .as_ref()
            .is_some_and(|s| s.conflict.is_none())
    );

    // The pre-rebase twin: the winner's patch replayed on the old base.
    run_git(
        &origin.sideline,
        &[
            "fetch",
            "-q",
            fx.repo.to_str().unwrap(),
            &format!("refs/heads/{branch}"),
        ],
    );
    run_git(&origin.sideline, &["checkout", "-q", "--detach", &base]);
    run_git(&origin.sideline, &["cherry-pick", "FETCH_HEAD"]);
    let twin = rev(&origin.sideline, "HEAD");
    (runner, branch, twin)
}

common::e2e! {
async fn a_resume_after_a_local_only_rebase_pushes_it_with_a_lease() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    let origin = wire_origin(&fx);
    let (mut runner, branch, twin) = rebased_locally_only(&fx, &origin).await;
    let bare = fx.tmp.path().join("origin.git");
    run_git(&origin.sideline, &["push", "-q", "origin", &format!("{twin}:refs/heads/{branch}")]);
    assert_ne!(rev(&bare, &format!("refs/heads/{branch}")), rev(&fx.repo, &format!("refs/heads/{branch}")));

    runner.execute().await.expect("resume");
    assert!(
        runner.state.base_sync.as_ref().is_some_and(|s| s.conflict.is_none()),
        "{:?}",
        runner.state.base_sync
    );
    assert_eq!(
        rev(&bare, &format!("refs/heads/{branch}")),
        rev(&fx.repo, &format!("refs/heads/{branch}")),
        "the locally rebased tip must reach the remote on resume"
    );
}
}

/// Two-commit winner: `note.txt` (conflicts with upstream's) then
/// `second.txt` (does not).
async fn two_commit_run(
    fx: &mut common::Fixture,
    skip_one: bool,
    second_upstream: bool,
    third_commit: bool,
) -> Runner {
    fx.config.graph.candidates = 1;
    set_agent_env(fx, "MOCK_IMPL_TWO_COMMITS");
    if third_commit {
        set_agent_env(fx, "MOCK_IMPL_THIRD_COMMIT");
    }
    if skip_one {
        set_agent_env(fx, "MOCK_REBASE_FIX_SKIP_ONE");
    }
    let origin = wire_origin(fx);
    let mut runner = Runner::start(
        &fx.repo,
        "create note.txt".to_owned(),
        fx.config.clone(),
        magi::run::Origin::operator(),
    )
    .await
    .expect("start");
    if second_upstream {
        std::fs::write(origin.sideline.join("second.txt"), "second\n").unwrap();
    }
    land_on_origin(&origin.sideline, "note.txt", "upstream\n");
    runner.execute().await.expect("execute");
    runner
}

common::e2e! {
async fn a_fixer_that_skips_one_of_several_commits_is_a_failed_rebase() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    let runner = two_commit_run(&mut fx, true, false, false).await;
    let state = &runner.state;
    assert_eq!(state.status, RunStatus::Blocked, "{:?}", state.events);
    let why = state
        .base_sync
        .as_ref()
        .and_then(|s| s.conflict.clone())
        .expect("a reason is recorded");
    assert!(why.contains("dropped"), "{why}");
    assert!(why.contains("add note from"), "{why}");
    let winner = state.winner().expect("a winner");
    assert!(
        !is_ancestor(&fx.repo, "origin/main", &winner.branch),
        "the branch keeps its own commits"
    );
}
}

common::e2e! {
async fn a_resume_with_nothing_to_push_pushes_nothing() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    let origin = wire_origin(&fx);
    let (mut runner, branch, _twin) = rebased_locally_only(&fx, &origin).await;
    let bare = fx.tmp.path().join("origin.git");
    let pushed = |r: &Runner| r.state.events.iter().filter(|e| e.message.contains("pushed rebased")).count();

    // No remote copy of the branch at all.
    runner.execute().await.expect("resume");
    assert_eq!(pushed(&runner), 0);

    // The remote copy is already the local tip.
    run_git(&fx.repo, &["push", "-q", "origin", &format!("refs/heads/{branch}:refs/heads/{branch}")]);
    let before = rev(&bare, &format!("refs/heads/{branch}"));
    runner.execute().await.expect("resume");
    assert_eq!(pushed(&runner), 0);
    assert_eq!(rev(&bare, &format!("refs/heads/{branch}")), before);
    assert!(runner.state.base_sync.as_ref().is_some_and(|s| s.conflict.is_none()));
}
}

common::e2e! {
async fn resolving_every_commit_of_a_multi_commit_winner_is_applied() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    let runner = two_commit_run(&mut fx, false, false, false).await;
    let state = &runner.state;
    let sync = state.base_sync.as_ref().expect("base sync recorded");
    assert!(sync.conflict.is_none(), "{:?}", sync.conflict);
    assert_eq!(state.status, RunStatus::Ready, "{:?}", state.events);
    let winner = state.winner().expect("a winner");
    assert!(winner.worktree.join("second.txt").exists());
    assert!(winner.worktree.join("note.txt").exists());
}
}

common::e2e! {
async fn a_resume_over_a_foreign_push_to_the_branch_stops_without_pushing() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    fx.config.graph.candidates = 1;
    let origin = wire_origin(&fx);
    let (mut runner, branch, twin) = rebased_locally_only(&fx, &origin).await;
    let bare = fx.tmp.path().join("origin.git");

    // A person's commit on top of the twin: not contained, not a patch twin.
    std::fs::write(origin.sideline.join("theirs.txt"), "x\n").unwrap();
    run_git(&origin.sideline, &["add", "-A"]);
    run_git(&origin.sideline, &["commit", "-q", "-m", "a person's commit"]);
    run_git(&origin.sideline, &["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")]);
    let theirs = rev(&bare, &format!("refs/heads/{branch}"));
    assert_ne!(theirs, twin);

    runner.execute().await.expect("resume");
    assert_eq!(runner.state.status, RunStatus::Blocked);
    let why = runner.state.base_sync.as_ref().and_then(|s| s.conflict.clone()).expect("a reason");
    assert!(why.contains("does not contain"), "{why}");
    assert_eq!(rev(&bare, &format!("refs/heads/{branch}")), theirs, "their commit must survive");
}
}

common::e2e! {
async fn a_commit_already_on_the_base_is_not_reported_as_dropped() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    let runner = two_commit_run(&mut fx, false, true, false).await;
    let state = &runner.state;
    let sync = state.base_sync.as_ref().expect("base sync recorded");
    assert!(sync.conflict.is_none(), "{:?}", sync.conflict);
    assert_eq!(state.status, RunStatus::Ready, "{:?}", state.events);
}
}

common::e2e! {
async fn an_upstream_commit_that_a_later_commit_revises_is_not_reported_as_dropped() {
    let _home = home_lock().await;
    let mut fx = fixture(_home, Judges::Unanimous, false);
    let runner = two_commit_run(&mut fx, false, true, true).await;
    let state = &runner.state;
    let sync = state.base_sync.as_ref().expect("base sync recorded");
    assert!(sync.conflict.is_none(), "{:?}", sync.conflict);
    assert_eq!(state.status, RunStatus::Ready, "{:?}", state.events);
    let winner = state.winner().expect("a winner");
    let second = std::fs::read_to_string(winner.worktree.join("second.txt")).unwrap();
    assert!(second.contains("revised"), "{second}");
}
}
