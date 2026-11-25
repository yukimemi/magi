//! Deterministic build identity shared by the `magi` library and CLI binary.
//!
//! Integration tests use this identity to verify that a spawned CLI or worker
//! executable actually matches the library linked into the test process, before
//! any state store is read or modified.

use std::ffi::OsStr;
use std::path::Path;

/// Deterministic identity of this compiled `magi` build (`<version>-<sha256>`).
pub const BUILD_ID: &str = env!("MAGI_BUILD_ID");

/// Hidden argv flag that prints [`BUILD_ID`] and exits before initializing any
/// runtime or state store.
pub const PROBE_ARG: &str = "__magi_build_id";

/// Environment variable that binds a spawned `magi` CLI (and its child workers)
/// to an exact [`BUILD_ID`], failing closed before touching state on mismatch.
pub const EXPECT_ENV: &str = "MAGI_EXPECT_BUILD_ID";

/// Handle the hidden `magi __magi_build_id` probe before any startup side effects.
///
/// Returns `true` if the probe flag was handled and the process should exit `0`.
pub fn handle_probe_arg() -> bool {
    let mut args = std::env::args_os();
    let _exe = args.next();
    if args.next().is_some_and(|arg| arg == PROBE_ARG) && args.next().is_none() {
        println!("{BUILD_ID}");
        return true;
    }
    false
}

/// Fail closed when [`EXPECT_ENV`] is set and does not equal [`BUILD_ID`].
///
/// Production invocations do not set [`EXPECT_ENV`] and proceed normally.
pub fn verify_expected_env() -> Result<(), String> {
    let expected = std::env::var_os(EXPECT_ENV);
    let executable = std::env::current_exe().unwrap_or_else(|_| Path::new("<unknown>").into());
    check_expected(expected.as_deref(), BUILD_ID, &executable)
}

fn check_expected(expected: Option<&OsStr>, actual: &str, executable: &Path) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if expected.to_str() == Some(actual) {
        return Ok(());
    }
    Err(format!(
        "magi build-identity mismatch for {}: expected {EXPECT_ENV}={:?}, actual {actual}",
        executable.display(),
        expected.to_string_lossy(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_contains_package_version_and_sha256_hex() {
        let prefix = format!("{}-", env!("CARGO_PKG_VERSION"));
        let digest = BUILD_ID
            .strip_prefix(&prefix)
            .unwrap_or_else(|| panic!("expected prefix {prefix:?} in {BUILD_ID:?}"));
        assert_eq!(digest.len(), 64, "{BUILD_ID}");
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "digest must be lowercase hex: {BUILD_ID}"
        );
    }

    #[test]
    fn check_expected_allows_unset_and_exact_match_and_rejects_mismatch() {
        let exe = Path::new("/example/magi");
        assert!(check_expected(None, "0.255.0-abc", exe).is_ok());
        assert!(check_expected(Some(OsStr::new("0.255.0-abc")), "0.255.0-abc", exe).is_ok());

        let empty_err = check_expected(Some(OsStr::new("")), "0.255.0-abc", exe).unwrap_err();
        assert!(empty_err.contains("0.255.0-abc"), "{empty_err}");
        assert!(empty_err.contains("/example/magi"), "{empty_err}");

        let mismatch_err =
            check_expected(Some(OsStr::new("0.255.0-expected")), "0.255.0-actual", exe)
                .unwrap_err();
        assert!(mismatch_err.contains("0.255.0-expected"), "{mismatch_err}");
        assert!(mismatch_err.contains("0.255.0-actual"), "{mismatch_err}");
        assert!(mismatch_err.contains("/example/magi"), "{mismatch_err}");
    }
}
