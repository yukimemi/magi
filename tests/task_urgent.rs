//! The CLI reports the persisted urgent setting in both mutation and show.

use std::process::Command;

use magi::queue::{Queue, Source, Task};

#[test]
fn urgent_command_and_show_report_saved_state() {
    let home = tempfile::tempdir().unwrap();
    let queue = Queue::at(home.path().join("queue"));
    let mut task = Task::new(
        "urgent output".into(),
        "preserve instruction".into(),
        home.path().into(),
        Source::Human,
    );
    queue.put(&mut task).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_magi"))
            .args(args)
            .env("MAGI_HOME", home.path())
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    };
    for (clear, state) in [(false, "enabled"), (true, "disabled")] {
        let mut args = vec!["task", "urgent", task.short()];
        if clear {
            args.push("--clear");
        }
        let output = run(&args);
        assert!(output.status.success(), "{:?}", output);
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains(&format!("urgent now {state}"))
        );
        assert_eq!(queue.get(&task.id).unwrap().urgent, !clear);
        let output = run(&["task", "show", task.short()]);
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains(&format!("urgent    {state}"))
        );
    }
    let _claim = queue.claim(&task.id).unwrap();
    let output = run(&["task", "urgent", task.short()]);
    assert!(!output.status.success());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.contains("claimed") && error.contains("retry"),
        "{error}"
    );
    assert!(!queue.get(&task.id).unwrap().urgent);
}
