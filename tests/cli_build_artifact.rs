//! Shared-target CLI selection and build-identity pinning must preserve the integration test's build.
#[allow(dead_code)]
#[path = "common/cli.rs"]
mod cli;

use std::path::{Path, PathBuf};

fn artifact(profile: &Path, hash: &str, library: u64, rustc: u64) -> PathBuf {
    let fingerprint = profile
        .join(".fingerprint")
        .join(format!("magi-cli-{hash}"));
    std::fs::create_dir_all(&fingerprint).unwrap();
    std::fs::write(
        fingerprint.join("bin-magi.json"),
        serde_json::to_vec(&serde_json::json!({
            "rustc": rustc, "deps": [[1, "magi", false, library]],
        }))
        .unwrap(),
    )
    .unwrap();
    let executable = profile
        .join("deps")
        .join(format!("magi-{hash}{}", std::env::consts::EXE_SUFFIX));
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(&executable, "binary").unwrap();
    executable
}

fn copy_real_cli(dest: &Path) {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut input = std::fs::File::open(cli::magi_executable()).unwrap();
    let mut output = std::fs::File::create(dest).unwrap();
    std::io::copy(&mut input, &mut output).unwrap();
    drop(output);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn cli_selection_ignores_replaced_top_level_binary_and_other_builds() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path();
    let fingerprint = profile.join(".fingerprint/magi-cli-testhash");
    std::fs::create_dir_all(&fingerprint).unwrap();
    std::fs::write(
        fingerprint.join("test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    // A version string alone cannot distinguish two builds of the same release.
    artifact(profile, "old-library", 122, 42);
    artifact(profile, "other-toolchain", 123, 41);
    let expected = artifact(profile, "matching", 123, 42);
    std::fs::write(profile.join("magi"), "replaced by an unrelated build").unwrap();
    let test = profile.join("deps/example-testhash");
    assert_eq!(
        cli::matching_cli_for_layout(&test, true),
        Some(expected.clone())
    );
    std::fs::write(profile.join("magi"), "replaced again").unwrap();
    assert_eq!(cli::matching_cli_for_layout(&test, true), Some(expected));
}

#[test]
fn relocated_tests_without_build_metadata_can_use_cargos_executable() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(cli::matching_cli(&tmp.path().join("deps/example-testhash")).is_none());
}

#[test]
#[should_panic(expected = "no CLI artifact links this test's magi library")]
fn mismatched_local_build_does_not_fall_back_to_an_unrelated_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let fingerprint = tmp.path().join(".fingerprint/magi-cli-testhash");
    std::fs::create_dir_all(&fingerprint).unwrap();
    std::fs::write(
        fingerprint.join("test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    artifact(tmp.path(), "unrelated", 122, 42);
    cli::matching_cli_for_layout(&tmp.path().join("deps/example-testhash"), true);
}

#[test]
fn msvc_cli_selection_uses_unhashed_dependency_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path();
    let fingerprint = profile.join(".fingerprint/magi-cli-testhash");
    std::fs::create_dir_all(&fingerprint).unwrap();
    std::fs::write(
        fingerprint.join("test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    let hashed = artifact(profile, "matching", 123, 42);
    let expected = profile
        .join("deps")
        .join(format!("magi{}", std::env::consts::EXE_SUFFIX));
    std::fs::rename(hashed, &expected).unwrap();
    let test = profile.join("deps/example-testhash");
    assert_eq!(
        cli::matching_cli_for_layout(&test, false),
        Some(expected.clone())
    );
    // The ordinary profile-level executable is never used as a substitute.
    std::fs::write(profile.join("magi"), "unrelated build").unwrap();
    assert_eq!(cli::matching_cli_for_layout(&test, false), Some(expected));
}

#[test]
#[should_panic(expected = "no CLI artifact links this test's magi library")]
fn msvc_cli_selection_rejects_an_unrelated_library() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path();
    let fingerprint = profile.join(".fingerprint/magi-cli-testhash");
    std::fs::create_dir_all(&fingerprint).unwrap();
    std::fs::write(
        fingerprint.join("test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    let hashed = artifact(profile, "unrelated", 122, 42);
    let executable = profile
        .join("deps")
        .join(format!("magi{}", std::env::consts::EXE_SUFFIX));
    std::fs::rename(hashed, executable).unwrap();
    cli::matching_cli_for_layout(&profile.join("deps/example-testhash"), false);
}

fn build_dir_artifact(
    profile: &Path,
    hash: &str,
    library: u64,
    rustc: u64,
    hashed_executable: bool,
) -> PathBuf {
    let unit = profile.join("build").join("magi-cli").join(hash);
    let fingerprint = unit.join("fingerprint");
    std::fs::create_dir_all(&fingerprint).unwrap();
    std::fs::write(
        fingerprint.join("bin-magi.json"),
        serde_json::to_vec(&serde_json::json!({
            "rustc": rustc, "deps": [[1, "magi", false, library]],
        }))
        .unwrap(),
    )
    .unwrap();
    let name = if hashed_executable {
        format!("magi-{hash}{}", std::env::consts::EXE_SUFFIX)
    } else {
        format!("magi{}", std::env::consts::EXE_SUFFIX)
    };
    let executable = unit.join("out").join(name);
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(&executable, "binary").unwrap();
    executable
}

#[test]
fn build_dir_cli_selection_matches_test_library_and_ignores_top_level_binary() {
    for hashed in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let profile = tmp.path();
        let test_unit = profile.join("build/magi-cli/testhash");
        let fingerprint = test_unit.join("fingerprint");
        std::fs::create_dir_all(&fingerprint).unwrap();
        std::fs::write(
            fingerprint.join("test-integration-test-example.json"),
            r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
        )
        .unwrap();
        build_dir_artifact(profile, "old-library", 122, 42, hashed);
        build_dir_artifact(profile, "other-toolchain", 123, 41, hashed);
        let expected = build_dir_artifact(profile, "matching", 123, 42, hashed);
        std::fs::write(
            profile.join(format!("magi{}", std::env::consts::EXE_SUFFIX)),
            "replaced by an unrelated build",
        )
        .unwrap();
        let test = test_unit
            .join("out")
            .join(format!("example-testhash{}", std::env::consts::EXE_SUFFIX));
        assert_eq!(
            cli::matching_cli_for_layout(&test, hashed),
            Some(expected.clone())
        );
    }
}

#[test]
#[should_panic(expected = "no CLI artifact links this test's magi library")]
fn build_dir_mismatched_local_build_does_not_fall_back_to_top_level_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path();
    let test_unit = profile.join("build/magi-cli/testhash");
    let fingerprint = test_unit.join("fingerprint");
    std::fs::create_dir_all(&fingerprint).unwrap();
    std::fs::write(
        fingerprint.join("test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    build_dir_artifact(profile, "unrelated", 122, 42, false);
    std::fs::write(
        profile.join(format!("magi{}", std::env::consts::EXE_SUFFIX)),
        "unrelated top-level build",
    )
    .unwrap();
    let test = test_unit
        .join("out")
        .join(format!("example-testhash{}", std::env::consts::EXE_SUFFIX));
    cli::matching_cli_for_layout(&test, false);
}

#[test]
fn msvc_shared_deps_executable_with_matching_fingerprint_fails_closed_on_build_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path();
    let test_fp = profile.join(".fingerprint/magi-cli-testhash");
    std::fs::create_dir_all(&test_fp).unwrap();
    std::fs::write(
        test_fp.join("test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    let bin_fp = profile.join(".fingerprint/magi-cli-matching");
    std::fs::create_dir_all(&bin_fp).unwrap();
    std::fs::write(
        bin_fp.join("bin-magi.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    let shared_deps_exe = profile
        .join("deps")
        .join(format!("magi{}", std::env::consts::EXE_SUFFIX));
    copy_real_cli(&shared_deps_exe);

    let test = profile.join(format!(
        "deps/example-testhash{}",
        std::env::consts::EXE_SUFFIX
    ));
    let pin_root = tmp.path().join("pins");

    // 1. Fingerprint still matches, but the shared unhashed deps/magi.exe reports
    //    a different build identity (e.g. schema 24 vs schema 23).
    let expected = "0.255.0-0000000000000000000000000000000000000000000000000000000000000023";
    let err = cli::resolve_and_pin_cli(&test, false, &shared_deps_exe, Some(&pin_root), expected)
        .unwrap_err();
    assert!(err.contains(expected), "{err}");
    assert!(err.contains(magi::build_id::BUILD_ID), "{err}");
    assert!(
        err.contains(&shared_deps_exe.display().to_string()),
        "{err}"
    );

    // 2. Replace the shared unhashed deps/magi.exe with a binary that does not
    //    implement the build-identity probe (e.g. an older build) while leaving
    //    the matching fingerprint intact.
    std::fs::copy(std::env::current_exe().unwrap(), &shared_deps_exe).unwrap();
    let err = cli::resolve_and_pin_cli(
        &test,
        false,
        &shared_deps_exe,
        Some(&pin_root),
        magi::build_id::BUILD_ID,
    )
    .unwrap_err();
    assert!(err.contains(magi::build_id::BUILD_ID), "{err}");
    assert!(
        err.contains(&shared_deps_exe.display().to_string()),
        "{err}"
    );
}

#[test]
fn pinned_cli_remains_immutable_after_shared_deps_executable_is_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let profile = tmp.path();
    let test_fp = profile.join(".fingerprint/magi-cli-testhash");
    std::fs::create_dir_all(&test_fp).unwrap();
    std::fs::write(
        test_fp.join("test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    let bin_fp = profile.join(".fingerprint/magi-cli-matching");
    std::fs::create_dir_all(&bin_fp).unwrap();
    std::fs::write(
        bin_fp.join("bin-magi.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    let shared_deps_exe = profile
        .join("deps")
        .join(format!("magi{}", std::env::consts::EXE_SUFFIX));
    copy_real_cli(&shared_deps_exe);

    let test = profile.join(format!(
        "deps/example-testhash{}",
        std::env::consts::EXE_SUFFIX
    ));
    let pin_root = tmp.path().join("pins");
    let pinned = cli::resolve_and_pin_cli(
        &test,
        false,
        &shared_deps_exe,
        Some(&pin_root),
        magi::build_id::BUILD_ID,
    )
    .expect("matching MSVC executable should pin and verify");
    assert_ne!(pinned, shared_deps_exe);
    assert!(pinned.starts_with(&pin_root), "{}", pinned.display());

    // Simulate another build overwriting the shared unhashed deps/magi.exe after pinning.
    std::fs::copy(std::env::current_exe().unwrap(), &shared_deps_exe).unwrap();

    // The pinned copy still executes the verified original build.
    assert_eq!(
        cli::probe_build_id(&pinned).unwrap(),
        magi::build_id::BUILD_ID
    );
    let out = std::process::Command::new(&pinned)
        .arg("--version")
        .env(magi::build_id::EXPECT_ENV, magi::build_id::BUILD_ID)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A fresh resolution against the overwritten shared path now fails closed.
    let err = cli::resolve_and_pin_cli(
        &test,
        false,
        &shared_deps_exe,
        Some(&pin_root),
        magi::build_id::BUILD_ID,
    )
    .unwrap_err();
    assert!(
        err.contains(&shared_deps_exe.display().to_string()),
        "{err}"
    );
}

#[test]
fn build_dir_and_relocated_nextest_fallbacks_verify_and_pin_build_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let pin_root = tmp.path().join("pins");

    // 1. Build-dir layout verifies actual build identity, not only bin-magi.json.
    let profile = tmp.path().join("target-build-dir/debug");
    let test_unit = profile.join("build/magi-cli/testhash");
    std::fs::create_dir_all(test_unit.join("fingerprint")).unwrap();
    std::fs::write(
        test_unit.join("fingerprint/test-integration-test-example.json"),
        r#"{"rustc":42,"deps":[[1,"magi",false,123]]}"#,
    )
    .unwrap();
    let unit_exe = build_dir_artifact(&profile, "matching", 123, 42, false);
    copy_real_cli(&unit_exe);
    let test = test_unit
        .join("out")
        .join(format!("example-testhash{}", std::env::consts::EXE_SUFFIX));
    let pinned_build_dir = cli::resolve_and_pin_cli(
        &test,
        false,
        Path::new("nonexistent-fallback"),
        Some(&pin_root),
        magi::build_id::BUILD_ID,
    )
    .expect("matching build-dir executable should verify");
    assert_eq!(
        cli::probe_build_id(&pinned_build_dir).unwrap(),
        magi::build_id::BUILD_ID
    );
    let mismatch = cli::resolve_and_pin_cli(
        &test,
        false,
        Path::new("nonexistent-fallback"),
        Some(&pin_root),
        "0.255.0-other-build",
    )
    .unwrap_err();
    assert!(mismatch.contains("0.255.0-other-build"), "{mismatch}");
    assert!(mismatch.contains(magi::build_id::BUILD_ID), "{mismatch}");

    // 2. Relocated nextest archive (no Cargo fingerprints) pins and verifies fallback.
    let relocated = tmp.path().join("nextest-archive");
    let relocated_test = relocated.join(format!(
        "deps/example-testhash{}",
        std::env::consts::EXE_SUFFIX
    ));
    let fallback_cli = relocated.join(format!("magi{}", std::env::consts::EXE_SUFFIX));
    copy_real_cli(&fallback_cli);

    let pinned_fallback = cli::resolve_and_pin_cli(
        &relocated_test,
        false,
        &fallback_cli,
        Some(&pin_root),
        magi::build_id::BUILD_ID,
    )
    .expect("relocated nextest fallback should pin and verify");
    // Content-addressed pin is reused for the identical binary bytes.
    assert_eq!(pinned_fallback, pinned_build_dir);

    let relocated_mismatch = cli::resolve_and_pin_cli(
        &relocated_test,
        false,
        &fallback_cli,
        Some(&pin_root),
        "0.255.0-relocated-wrong",
    )
    .unwrap_err();
    assert!(
        relocated_mismatch.contains("0.255.0-relocated-wrong"),
        "{relocated_mismatch}"
    );
    assert!(
        relocated_mismatch.contains(&fallback_cli.display().to_string()),
        "{relocated_mismatch}"
    );

    // Overwriting the fallback after pinning does not affect the pinned copy.
    std::fs::write(&fallback_cli, b"replaced-relocated-binary").unwrap();
    assert_eq!(
        cli::probe_build_id(&pinned_fallback).unwrap(),
        magi::build_id::BUILD_ID
    );
}

#[test]
fn concurrent_pinning_reuses_one_content_addressed_copy_without_staging_leaks() {
    let tmp = tempfile::tempdir().unwrap();
    let pin_root = tmp.path().join("concurrent-pins");
    let source = cli::magi_executable().to_path_buf();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let source = source.clone();
            let pin_root = pin_root.clone();
            std::thread::spawn(move || cli::pin_executable(&source, &pin_root))
        })
        .collect();
    let results: Vec<PathBuf> = handles
        .into_iter()
        .map(|h| h.join().unwrap().expect("concurrent pin must succeed"))
        .collect();

    for path in &results {
        assert_eq!(path, &results[0]);
        assert!(path.is_file(), "{}", path.display());
    }
    let entries: Vec<_> = std::fs::read_dir(&pin_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 1, "expected one generation dir: {entries:?}");
    assert!(
        !entries[0].starts_with(".staging-"),
        "staging dir leaked: {entries:?}"
    );
}

#[test]
fn old_pin_survives_other_generations_between_child_launches() {
    let tmp = tempfile::tempdir().unwrap();
    let pin_root = tmp.path().join("pins");
    let source = tmp.path().join("source");
    std::fs::write(&source, b"original generation").unwrap();
    let original = cli::pin_executable(&source, &pin_root).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    let generation = original.parent().unwrap();
    // Age the legacy usage marker without sleeping.
    let marker = std::fs::File::create(generation.join(".used")).unwrap();
    marker.set_modified(old).unwrap();
    drop(marker);

    for n in 0..10 {
        std::fs::write(&source, format!("other generation {n}")).unwrap();
        cli::pin_executable(&source, &pin_root).unwrap();
    }
    assert_eq!(std::fs::read(original).unwrap(), b"original generation");
}

#[test]
fn build_identity_mismatch_refuses_cli_before_mutating_private_queue() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("magi-home");
    let queue = magi::queue::Queue::at(home.join("queue"));
    let mut task = magi::queue::Task::new(
        "Protected task".into(),
        "must stay untouched on build mismatch".into(),
        tmp.path().to_path_buf(),
        magi::queue::Source::Human,
    );
    queue.put(&mut task).unwrap();
    let task_path = queue.path_of(&task.id);
    let before = std::fs::read(&task_path).unwrap();

    // Verify magi_cli_env propagates both the verified executable and expected build id.
    let nested_env = cli::magi_cli_env(&home, "TEST_CLI_BIN");
    assert_eq!(
        nested_env
            .get(magi::build_id::EXPECT_ENV)
            .map(String::as_str),
        Some(magi::build_id::BUILD_ID)
    );
    assert_eq!(
        nested_env.get("TEST_CLI_BIN").map(Path::new),
        Some(cli::magi_executable())
    );

    let wrong = "0.255.0-expected-other-build";
    let output = cli::magi_cli(&home)
        .env(magi::build_id::EXPECT_ENV, wrong)
        .args(["task", "hold", &task.id, "must not apply"])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "CLI must fail closed when {} mismatches",
        magi::build_id::EXPECT_ENV
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(wrong), "{stderr}");
    assert!(stderr.contains(magi::build_id::BUILD_ID), "{stderr}");
    assert!(
        stderr.contains(&cli::magi_executable().display().to_string()),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read(&task_path).unwrap(),
        before,
        "private queue task must remain untouched on build-identity mismatch"
    );
}

#[test]
fn current_build_layout_resolves_and_pins_a_matching_cli_artifact() {
    let test = std::env::current_exe().expect("test executable");
    let has_metadata = test
        .parent()
        .and_then(Path::parent)
        .is_some_and(|dir| dir.join("fingerprint").is_dir() || dir.join(".fingerprint").is_dir());
    if has_metadata {
        let resolved = cli::matching_cli(&test)
            .expect("local Cargo build should resolve a library-matched CLI artifact");
        assert!(resolved.is_file(), "{}", resolved.display());
        assert_ne!(
            resolved,
            PathBuf::from(env!("CARGO_BIN_EXE_magi")),
            "resolved CLI must come from the library-matched unit directory, not the shared top-level binary"
        );
    }
    let pinned = cli::magi_executable();
    assert!(pinned.is_file(), "{}", pinned.display());
    assert_ne!(
        pinned,
        PathBuf::from(env!("CARGO_BIN_EXE_magi")),
        "pinned CLI must be an immutable copy, not the shared top-level binary"
    );
    assert_eq!(
        cli::probe_build_id(pinned).unwrap(),
        magi::build_id::BUILD_ID
    );
}
