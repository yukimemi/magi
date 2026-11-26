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

fn read_repo_file(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Extract the YAML block for a single job under `jobs:` in `.github/workflows/ci.yml`.
fn ci_job_block(ci_yml: &str, job_id: &str) -> String {
    let header = format!("  {job_id}:");
    let mut lines = ci_yml.lines();
    let mut found = false;
    let mut block = Vec::new();
    for line in &mut lines {
        if line == header {
            found = true;
            block.push(line);
            break;
        }
    }
    assert!(
        found,
        "missing CI job `{job_id}` in .github/workflows/ci.yml"
    );
    for line in lines {
        // Next top-level job under `jobs:` starts with exactly two spaces and an identifier.
        if line.starts_with("  ")
            && !line.starts_with("   ")
            && !line.trim_start().starts_with('#')
            && line.ends_with(':')
        {
            break;
        }
        block.push(line);
    }
    block.join("\n")
}

#[test]
fn local_and_ci_nextest_unit_configuration_stay_aligned() {
    let makefile = table("Makefile.toml");
    let vars = table(".kata/vars.toml");

    let make_runner = makefile["env"]["PJ_RUST_TEST_RUNNER"]
        .as_str()
        .expect("Makefile.toml [env].PJ_RUST_TEST_RUNNER");
    let kata_runner = vars["PJ_RUST_TEST_RUNNER"]
        .as_str()
        .expect(".kata/vars.toml top-level PJ_RUST_TEST_RUNNER");
    assert_eq!(
        make_runner, kata_runner,
        "Makefile.toml [env] and .kata/vars.toml must select the same PJ_RUST_TEST_RUNNER"
    );
    assert_eq!(
        make_runner, "nextest-unit",
        "magi opts into PJ_RUST_TEST_RUNNER = \"nextest-unit\""
    );

    let test_targets_script = makefile["tasks"]["test-targets"]["script"]
        .as_str()
        .expect("Makefile.toml tasks.test-targets.script");
    assert!(
        test_targets_script.contains("if eq ${runner} \"nextest-unit\""),
        "Makefile.toml test-targets must support nextest-unit"
    );
    assert!(
        test_targets_script.contains(
            "cargo nextest run --locked --no-tests=pass -E \"kind(lib) | kind(bin) | kind(test)\""
        ),
        "Makefile.toml test-targets must run lib/bin/integration in one nextest invocation"
    );
    assert!(
        test_targets_script.contains("cargo test --locked --examples --benches"),
        "Makefile.toml test-targets must run examples and benches via cargo test"
    );

    // Prevent `cargo test --locked --examples --benches` from re-running lib/bin
    // unit tests through Cargo's default benchmark harnesses.
    let cargo = table("Cargo.toml");
    assert_eq!(
        cargo["lib"]["bench"].as_bool(),
        Some(false),
        "Cargo.toml [lib] must set bench = false to prevent unit test double execution"
    );
    let bins = cargo["bin"].as_array().expect("Cargo.toml [[bin]]");
    assert!(
        !bins.is_empty(),
        "Cargo.toml must declare at least one [[bin]]"
    );
    for bin in bins {
        assert_eq!(
            bin["bench"].as_bool(),
            Some(false),
            "every Cargo.toml [[bin]] must set bench = false to prevent unit test double execution"
        );
    }

    // Keep local and CI nextest retries equal and JUnit output path intact.
    let nextest = table(".config/nextest.toml");
    let default_retries = nextest["profile"]["default"]["retries"]
        .as_integer()
        .expect(".config/nextest.toml profile.default.retries");
    let ci_retries = nextest["profile"]["ci"]["retries"]
        .as_integer()
        .expect(".config/nextest.toml profile.ci.retries");
    assert_eq!(
        default_retries, ci_retries,
        "local and CI nextest profiles must keep equal retry counts"
    );
    assert_eq!(
        nextest["profile"]["ci"]["junit"]["path"].as_str(),
        Some("junit.xml"),
        ".config/nextest.toml must write CI JUnit to junit.xml"
    );

    // `.github/workflows/ci.yml` is ignored by kata and maintained manually;
    // keep its split synchronized with the `nextest-unit` template contract.
    let ci_yml = read_repo_file(".github/workflows/ci.yml");
    assert!(
        !ci_yml.contains("cargo test --lib --bins"),
        "CI must not keep the legacy cargo test --lib --bins command"
    );
    assert!(
        !ci_yml.contains("--test '*'"),
        "CI must filter integration targets via -E 'kind(test)', not --test '*'"
    );

    let unix_test = ci_job_block(&ci_yml, "test");
    assert!(
        unix_test.contains(
            "cargo nextest run --locked --no-tests=pass --profile ci -E 'kind(lib) | kind(bin) | kind(test)'"
        ),
        "Linux/macOS test job must run lib/bin/integration in one nextest invocation"
    );
    assert!(
        unix_test.contains("cargo test --locked --examples --benches"),
        "Linux/macOS test job must run examples and benches"
    );
    assert!(
        unix_test.contains("cargo test --locked --doc"),
        "Linux/macOS test job must preserve doctests"
    );

    let win_unit = ci_job_block(&ci_yml, "test-windows-unit");
    assert!(
        win_unit.contains(
            "cargo nextest run --locked --no-tests=pass --profile ci -E 'kind(lib) | kind(bin)'"
        ),
        "test-windows-unit must run lib/bin unit tests via nextest"
    );
    assert!(
        win_unit.contains("cargo test --locked --examples --benches"),
        "test-windows-unit must run examples and benches"
    );
    assert!(
        win_unit.contains("cargo test --locked --doc"),
        "test-windows-unit must preserve doctests"
    );
    assert!(
        win_unit.contains("name: junit-windows-unit"),
        "test-windows-unit must upload junit-windows-unit"
    );

    let win_shard = ci_job_block(&ci_yml, "test-windows-shard");
    assert!(
        win_shard.contains(
            "cargo nextest run --locked --no-tests=pass --profile ci -E 'kind(test)' --partition count:${{ matrix.shard }}/3"
        ),
        "test-windows-shard must run sharded kind(test) integration tests"
    );

    let win_agg = ci_job_block(&ci_yml, "test-windows");
    assert!(
        win_agg.contains("name: test (windows-latest)"),
        "Windows aggregate job must preserve the required check name `test (windows-latest)`"
    );
    assert!(
        win_agg.contains("needs: [test-windows-unit, test-windows-shard]"),
        "Windows aggregate job must aggregate test-windows-unit and test-windows-shard"
    );
}
