//! CLI surface for owner-selected immediate execution:
//! - `magi task add --immediate` (and coexistence with `--urgent`)
//! - `magi task immediate <id> [--clear]`
//! - `magi task list` / `magi task show [--json]` labels and admission status
//! - owner-only enforcement refusing agent seats and chats.

mod common;

use magi::daemon::{Status, write_status_to};
use magi::queue::{Queue, Source, Task};

#[test]
fn immediate_cli_add_toggle_show_list_and_owner_only_guards() {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let queue = Queue::at(home.path().join("queue"));

    let run = |args: &[&str], envs: &[(&str, &str)]| {
        let mut cmd = common::magi_cli(home.path());
        cmd.args(args)
            .env("NO_COLOR", "1")
            .env_remove("MAGI_RUN")
            .env_remove("MAGI_NODE")
            .env_remove("MAGI_CHAT_CONTROL_CHANNEL")
            .env_remove("MAGI_CHAT_CONTROL_SOCKET");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    };

    // Owner can file with both --immediate and --urgent.
    let added = run(
        &[
            "task",
            "add",
            "--repo",
            repo.to_str().unwrap(),
            "--immediate",
            "--urgent",
            "--json",
            "owner immediate task",
        ],
        &[],
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let filed: Task = serde_json::from_slice(&added.stdout).unwrap();
    assert!(filed.immediate);
    assert!(filed.urgent);

    // Without a live scheduler, show & list report waiting on scheduler, never "next poll".
    let show = run(&["task", "show", filed.short()], &[]);
    assert!(show.status.success());
    let show_text = String::from_utf8(show.stdout).unwrap();
    assert!(show_text.contains("immediate enabled"), "{show_text}");
    assert!(show_text.contains("urgent    enabled"), "{show_text}");
    assert!(
        show_text.contains("admission Immediate: waiting — scheduler not running"),
        "{show_text}"
    );

    let list = run(&["task", "list"], &[]);
    assert!(list.status.success());
    let list_text = String::from_utf8(list.stdout).unwrap();
    assert!(list_text.contains("[immediate]"), "{list_text}");
    assert!(
        list_text.contains("Immediate: waiting — scheduler not running"),
        "{list_text}"
    );

    // With a live scheduler heartbeat, show --json & show report "Immediate: next poll".
    let status = Status::new();
    write_status_to(&home.path().join("daemon.json"), &status).unwrap();
    let show_json = run(&["task", "show", filed.short(), "--json"], &[]);
    assert!(show_json.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&show_json.stdout).unwrap();
    assert_eq!(parsed["immediate"], true);
    assert_eq!(parsed["urgent"], true);
    assert_eq!(parsed["admission"]["mode"], "immediate");
    assert_eq!(parsed["admission"]["state"], "next_poll");
    assert_eq!(parsed["admission"]["label"], "Immediate: next poll");

    // Clearing immediate preserves urgent.
    let cleared = run(&["task", "immediate", filed.short(), "--clear"], &[]);
    assert!(
        cleared.status.success(),
        "{}",
        String::from_utf8_lossy(&cleared.stderr)
    );
    assert!(
        String::from_utf8(cleared.stdout)
            .unwrap()
            .contains("immediate now disabled")
    );
    let after_clear = queue.get(&filed.id).unwrap();
    assert!(!after_clear.immediate);
    assert!(after_clear.urgent);

    // Re-enabling immediate works on unclaimed queued task.
    let enabled = run(&["task", "immediate", filed.short()], &[]);
    assert!(enabled.status.success());
    assert!(queue.get(&filed.id).unwrap().immediate);

    // Claimed task refuses immediate change.
    let _claim = queue.claim(&filed.id).unwrap();
    let conflict = run(&["task", "immediate", filed.short(), "--clear"], &[]);
    assert!(!conflict.status.success());
    assert!(
        String::from_utf8_lossy(&conflict.stderr).contains("claimed"),
        "{}",
        String::from_utf8_lossy(&conflict.stderr)
    );
    drop(_claim);

    // Owner-only enforcement: agent seats and chat turns cannot add --immediate or toggle immediate.
    let mut existing = Task::new(
        "plain task".into(),
        "plain body".into(),
        repo.clone(),
        Source::Human,
    );
    queue.put(&mut existing).unwrap();
    for env_set in [
        &[("MAGI_RUN", "run-1"), ("MAGI_NODE", "implement")][..],
        &[("MAGI_RUN", "chat-1"), ("MAGI_NODE", "chat")][..],
        &[("MAGI_NODE", "external-chat")][..],
    ] {
        let refused_add = run(
            &[
                "task",
                "add",
                "--repo",
                repo.to_str().unwrap(),
                "--immediate",
                "agent immediate attempt",
            ],
            env_set,
        );
        assert!(
            !refused_add.status.success(),
            "agent/chat env {env_set:?} must not file --immediate"
        );
        let refused_toggle = run(&["task", "immediate", existing.short()], env_set);
        assert!(
            !refused_toggle.status.success(),
            "agent/chat env {env_set:?} must not enable immediate"
        );
        assert!(!queue.get(&existing.id).unwrap().immediate);
    }
}
