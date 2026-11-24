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
fn current_build_layout_resolves_a_matching_cli_artifact() {
    let test = std::env::current_exe().expect("test executable");
    let has_metadata = test
        .parent()
        .and_then(Path::parent)
        .is_some_and(|dir| dir.join("fingerprint").is_dir() || dir.join(".fingerprint").is_dir());
    if !has_metadata {
        return;
    }
    let resolved = cli::matching_cli(&test)
        .expect("local Cargo build should resolve a library-matched CLI artifact");
    assert!(resolved.is_file(), "{}", resolved.display());
    assert_ne!(
        resolved,
        PathBuf::from(env!("CARGO_BIN_EXE_magi")),
        "resolved CLI must come from the library-matched unit directory, not the shared top-level binary"
    );
}
