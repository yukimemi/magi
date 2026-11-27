mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use magi::cache::{self, AcquireOutcome, EntryStatus, Owner};
use magi::config::BuildCache;
use magi::queue::{Queue, TaskStatus};
use magi::run::RunState;

// Bound the whole CLI/worker lifecycle separately from the two-second cache
// wait under test. Git and worker startup can take much longer on Windows.
const REVIEW_BUDGET: std::time::Duration =
    std::time::Duration::from_secs(if cfg!(windows) { 360 } else { 100 });

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn relative_worktree_migration_does_not_lock_checkout_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.name", "test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    fs::write(repo.join("file"), "content\n").unwrap();
    git(&repo, &["add", "file"]);
    git(&repo, &["commit", "-m", "initial"]);
    git(&repo, &["config", "worktree.useRelativePaths", "true"]);
    let hooks = tmp.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    git(
        &repo,
        &[
            "config",
            "core.hooksPath",
            &hooks.to_string_lossy().replace('\\', "/"),
        ],
    );
    let hook = hooks.join("post-checkout");
    fs::write(
        &hook,
        "#!/bin/sh\ncase \"$PWD\" in */first) touch ../ready; while [ ! -f ../release ]; do sleep 0.1; done;; esac\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let first = tmp.path().join("first");
    let second = tmp.path().join("second");
    let other = async {
        let ready = tokio::time::timeout(REVIEW_BUDGET, async {
            while !tmp.path().join("ready").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await;
        let result = async {
            ready?;
            magi::git::enable_worktree_config(&repo).await?;
            magi::git::worktree_add_detached(&repo, &second, "HEAD").await
        }
        .await;
        // Release even on failure so the first Git process can exit.
        fs::write(tmp.path().join("release"), "").unwrap();
        result
    };
    let (first_result, second_result) = tokio::join!(
        magi::git::worktree_add_detached(&repo, &first, "HEAD"),
        other
    );
    first_result.unwrap();
    second_result.unwrap();
    assert!(
        fs::read_to_string(first.join(".git"))
            .unwrap()
            .starts_with("gitdir: ../")
    );
    assert_eq!(fs::read_to_string(second.join("file")).unwrap(), "content\n");
}

common::e2e! {
async fn isolated_reviews_build_their_own_sources_while_shared_cache_is_held() {
    let fx = common::fixture(common::home_lock().await, common::Judges::Unanimous, false);
    let home = fx.tmp.path().join("isolated home 日本語");
    let shared = fx.tmp.path().join("shared cache 日本語");
    let root = fx.repo.join("leaf crate");
    for value in ["first", "second", "shared"] {
        git(&fx.repo, &["checkout", "-b", value, "main"]);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("src/bin")).unwrap();
        fs::write(root.join("src/bin/nested.rs"), format!("fn main() {{ println!(\"nested {value}\"); }}\n")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("tests/build.rs"), r#"
#[test]
fn build_child() {
    assert!(std::process::Command::new("cargo")
        .args(["build", "--offline", "--locked", "--quiet", "--bin", "nested"])
        .status().unwrap().success());
}
"#).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname = 'same_package'\nversion = '0.1.0'\nedition = '2021'\n").unwrap();
        fs::write(root.join("Cargo.lock"), "version = 3\n\n[[package]]\nname = \"same_package\"\nversion = \"0.1.0\"\n").unwrap();
        fs::write(root.join("src/main.rs"), format!("fn main() {{ println!(\"{value}\"); }}\n")).unwrap();
        fs::write(fx.repo.join("note.txt"), value).unwrap();
        git(&fx.repo, &["add", "-A"]);
        git(&fx.repo, &["commit", "-m", value]);
    }
    git(&fx.repo, &["checkout", "main"]);
    git(&fx.repo, &["config", "worktree.useRelativePaths", "true"]);

    let reader = fx.tmp.path().join("reader.sh");
    fs::write(&reader, "[ -z \"${CARGO_TARGET_DIR+x}\" ] || exit 42\nexec sh \"$@\"\n").unwrap();
    let mut config = fx.config.clone();
    config.graph.timeout_resource_wait = 2;
    config.graph.timeout_verify = Some(40);
    config.disk.min_free_bytes = 1;
    for agent in &mut config.agents {
        agent.command.insert(1, reader.to_string_lossy().into_owned());
    }
    let shared_shell = shared.to_str().unwrap().replace('\\', "/");
    config.verify.e2e = vec![format!(
        "CARGO_TARGET_DIR='{shared_shell}' cargo run --offline --locked --quiet --bin same_package --manifest-path 'leaf crate/Cargo.toml'"
    )];
    config.verify.gate = vec![format!(
        "CARGO_TARGET_DIR='{shared_shell}' cargo check --offline --locked --manifest-path 'leaf crate/Cargo.toml' && \
         cargo test --offline --locked --manifest-path 'leaf crate/Cargo.toml' --test build"
    )];
    let config_path = fx.tmp.path().join("magi-config.toml");
    fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let owner = Owner::here("test", "fixture", "holder", &fx.repo, "");
    let AcquireOutcome::Acquired(shared_lease) = cache::try_acquire(&home, &shared, &owner).unwrap() else {
        panic!("fixture's unrelated shared cache was not free");
    };

    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from).or_else(|| dirs::home_dir().map(|path| path.join(".rustup"))).unwrap();
    let cli = || {
        let mut cli = tokio::process::Command::from(common::magi_cli(&home));
        cli.current_dir(&fx.repo)
            .env("RUSTUP_HOME", &rustup_home)
            .env("CARGO_HOME", fx.tmp.path().join("offline-cargo-home"))
            .env("CARGO_TARGET_DIR", &shared)
            .env("CARGO_NET_OFFLINE", "true")
            .stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        cli
    };
    let review = |branch, mode| {
        cli().args(["review", branch, "--force", "--merge", "none", "--build-cache", mode, "--config"])
            .arg(&config_path)
            .spawn().unwrap()
    };
    let urgent = cli()
        .args(["task", "add", "--force", "--urgent", "--json", "Review the second fixture"])
        .output().await.unwrap();
    assert!(urgent.status.success(), "{}", String::from_utf8_lossy(&urgent.stderr));
    let urgent: magi::queue::Task = serde_json::from_slice(&urgent.stdout).unwrap();
    let urgent_review = cli()
        .args(["review", "second", "--task", &urgent.id, "--merge", "none", "--config"])
        .arg(&config_path)
        .spawn().unwrap();
    let children = [review("first", "isolated"), urgent_review];
    for child in children {
        let out = tokio::time::timeout(REVIEW_BUDGET, child.wait_with_output())
            .await.unwrap_or_else(|_| {
                let runs: Vec<_> = Queue::at(home.join("queue")).list().into_iter().map(|task| {
                    let event = task.runs.last()
                        .and_then(|id| RunState::load_under(id, &home).ok())
                        .and_then(|mut run| run.events.pop())
                        .map(|event| event.message);
                    (task.review_of, task.status, event, task.last_error)
                }).collect();
                panic!("fixture review exceeded its budget: {runs:?}");
            }).unwrap();
        assert!(out.status.success(), "{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    }

    let mut targets = Vec::new();
    let tasks = Queue::at(home.join("queue")).list();
    assert_eq!(tasks.len(), 2);
    let inventory = cache::inventory(&home);
    for task in tasks {
        let branch = task.review_of.as_deref().unwrap();
        assert_eq!(task.status, TaskStatus::Done, "{branch}: {:?}", task.last_error);
        let run = RunState::load_under(&task.runs[0], &home).unwrap();
        assert_eq!(run.config.verify.build_cache, BuildCache::Isolated);
        let source = if branch == "second" { "urgent" } else { "task-override" };
        assert!(run.events.iter().any(|event| {
            event.node == "build-cache" && event.message == format!("isolated selected by {source}")
        }));
        let link = fs::read_to_string(run.candidates[0].worktree.join(".git")).unwrap();
        assert!(link.starts_with("gitdir: ../"), "{link}");
        assert!(run.reviews.last().unwrap().e2e.iter().any(|result| result.ok() && result.output_tail.trim() == branch));
        assert!(run.reviews.last().unwrap().reviews.iter().all(|seat| seat.failed.is_none()));
        assert!(run.gate.iter().all(|result| result.ok()));
        let target = inventory.iter().find_map(|entry| match &entry.status {
            EntryStatus::Idle(owner) if owner.run == run.id => Some(PathBuf::from(&entry.cache_dir)),
            _ => None,
        }).expect("admitted target was registered");
        let executable = target.join("debug").join(format!("same_package{}", std::env::consts::EXE_SUFFIX));
        let binary = Command::new(executable).output().unwrap();
        assert!(binary.status.success());
        assert_eq!(String::from_utf8(binary.stdout).unwrap().trim(), branch);
        let nested = target.join("debug").join(format!("nested{}", std::env::consts::EXE_SUFFIX));
        let nested = Command::new(nested).output().unwrap();
        assert!(nested.status.success());
        assert_eq!(String::from_utf8(nested.stdout).unwrap().trim(), format!("nested {branch}"));
        assert!(!run.candidates[0].worktree.join("leaf crate/target").exists());
        targets.push(target);
    }
    assert_ne!(targets[0], targets[1]);
    assert!(!shared.exists());
    assert!(cache::in_use(&home, &shared));
    config.verify.gate = vec![format!(
        "CARGO_TARGET_DIR='{shared_shell}' cargo check --offline --locked --manifest-path 'leaf crate/Cargo.toml'"
    )];
    fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let blocked = tokio::time::timeout(REVIEW_BUDGET, review("shared", "shared").wait_with_output())
        .await.expect("shared review did not report its resource wait").unwrap();
    assert!(!blocked.status.success());
    let queue = Queue::at(home.join("queue"));
    let task = queue.list().into_iter().find(|task| task.review_of.as_deref() == Some("shared")).unwrap();
    assert_eq!(task.status, TaskStatus::Held, "{:?}", task.last_error);
    assert_eq!(task.runs.len(), 1);
    let run_id = &task.runs[0];
    let blocked = RunState::load_under(run_id, &home).unwrap();
    assert_eq!(blocked.config.verify.build_cache, BuildCache::Shared);
    let block = blocked.resource_block.unwrap();
    assert_eq!(block.kind, "shared build cache");
    assert!(matches!(block.cause, magi::run::ResourceBlockCause::Timeout { .. }));
    assert!(!shared.exists());
    drop(shared_lease);
    let resumed = tokio::time::timeout(
        REVIEW_BUDGET,
        cli().args(["run", "--resume", run_id]).output(),
    ).await.expect("shared resume did not finish").unwrap();
    assert!(resumed.status.success(), "{}\n{}", String::from_utf8_lossy(&resumed.stdout), String::from_utf8_lossy(&resumed.stderr));
    let settled = queue.get(&task.id).unwrap();
    assert_eq!(settled.status, TaskStatus::Done, "{:?}", settled.last_error);
    assert_eq!(settled.runs, task.runs);
    assert_eq!(RunState::load_under(run_id, &home).unwrap().config.verify.build_cache, BuildCache::Shared);
    let binary_name = format!("same_package{}", std::env::consts::EXE_SUFFIX);
    let shared_binary = Command::new(shared.join("debug").join(&binary_name)).output().unwrap();
    assert!(shared_binary.status.success());
    assert_eq!(String::from_utf8(shared_binary.stdout).unwrap().trim(), "shared");

    let target = &targets[0];
    let AcquireOutcome::Acquired(lease) = cache::try_acquire(&home, target, &owner).unwrap() else {
        panic!("review did not release target lease");
    };
    assert!(matches!(cache::try_acquire(&home, target, &owner).unwrap(), AcquireOutcome::Busy(_)));
    assert!(cache::maintenance_prune(&home, target, 0).unwrap().is_none());
    drop(lease);
    assert!(cache::maintenance_prune(&home, target, 0).unwrap().unwrap().files > 0);
    let unregistered = home.join("isolated-builds").join("00000000-0000-0000-0000-000000000000").join("target");
    fs::create_dir_all(&unregistered).unwrap();
    fs::write(unregistered.join("keep"), "not magi output").unwrap();
    assert!(magi::build_target::check_prune(&home, target).unwrap());
    assert!(cache::maintenance_prune(&home, &unregistered, 0).is_err());
    assert!(unregistered.join("keep").is_file());
    assert!(!fx.repo.join("target").exists());
}
}
