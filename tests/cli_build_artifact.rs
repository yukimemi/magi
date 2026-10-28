//! Shared-target CLI selection must preserve the integration test's schema.
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
