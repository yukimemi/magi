//! `magi.toml` splits `cargo make check` between `[verify] e2e` (the tests) and
//! `[verify] gate` (everything else), so a run pays for the suite once per
//! commit instead of once in each. Nothing connects that split to
//! `Makefile.toml`: a check added to `tasks.check` and to neither half would be
//! enforced by CI alone, and a clean magi run would open a red pull request.

use std::collections::BTreeSet;
use std::path::Path;

fn table(file: &str) -> toml::Table {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .parse()
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

/// The cargo-make task each command of a `[verify]` array runs.
fn tasks(verify: &toml::Table, key: &str) -> Vec<String> {
    verify[key]
        .as_array()
        .unwrap_or_else(|| panic!("verify.{key} is not an array"))
        .iter()
        .map(|command| {
            let command = command.as_str().expect("a verify command is a string");
            let (_, task) = command
                .rsplit_once("cargo make ")
                .unwrap_or_else(|| panic!("verify.{key} `{command}` is not a cargo-make task"));
            task.to_owned()
        })
        .collect()
}

#[test]
fn e2e_and_gate_together_run_cargo_make_check_once() {
    let makefile = table("Makefile.toml");
    let check: BTreeSet<String> = makefile["tasks"]["check"]["dependencies"]
        .as_array()
        .expect("tasks.check.dependencies")
        .iter()
        .map(|task| task.as_str().expect("a task name").to_owned())
        .collect();

    let magi = table("magi.toml");
    let verify = magi["verify"].as_table().expect("[verify]");
    let e2e = tasks(verify, "e2e");
    let gate = tasks(verify, "gate");

    // The fixer is handed e2e's output during review, and e2e is what magi
    // runs again when the commit changes; the suite belongs there.
    assert!(
        e2e.contains(&"test".to_owned()),
        "e2e runs the tests: {e2e:?}"
    );

    let all: Vec<String> = e2e.into_iter().chain(gate).collect();
    let covered: BTreeSet<String> = all.iter().cloned().collect();
    assert_eq!(
        covered.len(),
        all.len(),
        "a task in both halves runs twice per commit: {all:?}"
    );
    assert_eq!(
        covered, check,
        "e2e and gate must add up to exactly what `cargo make check` runs"
    );
}
