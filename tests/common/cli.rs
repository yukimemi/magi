//! Isolation settings and build-identity pinning for real CLI integration subprocesses.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

// Mirror bridge::ENVIRONMENT: fixture CLIs must never borrow a standing
// conversation's authority from the process running the test suite.
pub(super) const CONTROL_ENV: [&str; 7] = [
    "MAGI_CHAT_CONTROL_FD",
    "MAGI_CHAT_CONTROL_RESPONSE_FD",
    "MAGI_CHAT_CONTROL_LOCK_FD",
    "MAGI_CHAT_CONTROL_IDENTITIES",
    "MAGI_CHAT_CONTROL_SOCKET",
    "MAGI_CHAT_CONTROL_PIPE",
    "MAGI_CHAT_CONTROL_CAP",
];

pub(super) const SEAT_ENV: [&str; 3] = ["MAGI_RUN", "MAGI_NODE", "MAGI_SEAT"];
// These resets apply to spawned CLIs. Library-embedding test hosts retain their
// explicit MAGI_WORKER_EXE when copying child isolation settings back to a host.
pub(super) const WORKER_ENV: [&str; 4] = magi::worker::INVOCATION_ENV;

const PIN_DIR_NAME: &str = "magi-test-cli";

static STAGING_NONCE: AtomicU64 = AtomicU64::new(0);

/// Cargo's unqualified `debug/magi` — and on legacy MSVC layouts, the unhashed
/// `debug/deps/magi.exe` — can be overwritten by another build sharing
/// `CARGO_TARGET_DIR` while these tests run. Pin the resolved candidate to a
/// content-addressed immutable copy and verify its embedded build identity
/// matches this test binary's `magi` library before touching any fixture state.
pub fn magi_executable() -> &'static Path {
    static EXE: LazyLock<PathBuf> = LazyLock::new(|| {
        let test = std::env::current_exe().expect("test executable");
        let fallback = fallback_cli_candidate();
        resolve_and_pin_cli(
            &test,
            !cfg!(target_env = "msvc"),
            &fallback,
            None,
            magi::build_id::BUILD_ID,
        )
        .unwrap_or_else(|error| panic!("{error}"))
    });
    &EXE
}

fn fallback_cli_candidate() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_magi")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_magi")))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LayoutCandidates {
    /// Cargo build metadata was found (build-dir or legacy layout).
    /// Fingerprint selection narrows candidates; it is not proof of identity.
    Local {
        profile: PathBuf,
        candidates: Vec<PathBuf>,
    },
    /// No Cargo build metadata was found (e.g. a relocated nextest archive).
    Relocated,
}

/// Nextest archives may omit Cargo fingerprints; in that case the caller falls
/// back to Cargo's supplied executable and still verifies its pinned copy's
/// build identity. A local build with fingerprints must find a matching
/// candidate rather than silently falling back to an unrelated top-level CLI.
pub(super) fn matching_cli(test: &Path) -> Option<PathBuf> {
    // Cargo omits extra-filename for ordinary local MSVC executables so their
    // embedded PDB path has no hash. Test executables still carry a hash.
    matching_cli_for_layout(test, !cfg!(target_env = "msvc"))
}

pub(super) fn matching_cli_for_layout(test: &Path, hashed_executable: bool) -> Option<PathBuf> {
    match resolve_layout_candidates(test, hashed_executable) {
        LayoutCandidates::Relocated => None,
        LayoutCandidates::Local { candidates, .. } => Some(
            candidates
                .into_iter()
                .next()
                .expect("no CLI artifact links this test's magi library; rebuild the magi binary"),
        ),
    }
}

pub(super) fn resolve_layout_candidates(test: &Path, hashed_executable: bool) -> LayoutCandidates {
    let Some((name, hash)) = test
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.rsplit_once('-'))
    else {
        return LayoutCandidates::Relocated;
    };
    let library = |value: &serde_json::Value| {
        value["deps"]
            .as_array()
            .and_then(|deps| deps.iter().find(|dep| dep[1] == "magi").cloned())
    };
    let test_fingerprint_name = format!("test-integration-test-{name}.json");

    // Cargo >= 1.100 build-dir layout:
    //   <profile>/build/<pkg>/<hash>/out/<test>-<hash><EXE_SUFFIX>
    //   <profile>/build/<pkg>/<hash>/fingerprint/test-integration-test-<name>.json
    //   <profile>/build/<pkg>/<bin-hash>/fingerprint/bin-magi.json
    //   <profile>/build/<pkg>/<bin-hash>/out/magi[-<bin-hash>]<EXE_SUFFIX>
    if test.parent().and_then(Path::file_name) == Some(std::ffi::OsStr::new("out"))
        && let Some(unit_dir) = test.parent().and_then(Path::parent)
        && let Some(pkg_build_dir) = unit_dir.parent()
        && let Ok(bytes) = std::fs::read(unit_dir.join("fingerprint").join(&test_fingerprint_name))
    {
        let test_fp: serde_json::Value =
            serde_json::from_slice(&bytes).expect("test build fingerprint");
        let dependency = library(&test_fp).expect("test links magi library");
        let profile = pkg_build_dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(pkg_build_dir)
            .to_path_buf();
        let mut entries: Vec<_> = std::fs::read_dir(pkg_build_dir)
            .expect("Cargo package build units")
            .map(|entry| entry.expect("Cargo build unit entry"))
            .collect();
        entries.sort_by_key(|entry| entry.file_name());

        let mut candidates = Vec::new();
        for entry in entries {
            let unit = entry.path();
            let Ok(bytes) = std::fs::read(unit.join("fingerprint").join("bin-magi.json")) else {
                continue;
            };
            let Ok(binary) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            if library(&binary).as_ref() != Some(&dependency) || binary["rustc"] != test_fp["rustc"]
            {
                continue;
            }
            let Some(bin_hash) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let out_dir = unit.join("out");
            let hashed = out_dir.join(format!("magi-{bin_hash}{}", std::env::consts::EXE_SUFFIX));
            let unhashed = out_dir.join(format!("magi{}", std::env::consts::EXE_SUFFIX));
            if hashed_executable && hashed.is_file() && !candidates.contains(&hashed) {
                candidates.push(hashed);
            }
            if unhashed.is_file() && !candidates.contains(&unhashed) {
                candidates.push(unhashed);
            }
        }
        return LayoutCandidates::Local {
            profile,
            candidates,
        };
    }

    // Legacy Cargo layout:
    //   <profile>/deps/<test>-<hash><EXE_SUFFIX>
    //   <profile>/.fingerprint/<pkg>-<hash>/test-integration-test-<name>.json
    //   <profile>/.fingerprint/<pkg>-<bin-hash>/bin-magi.json
    //   <profile>/deps/magi[-<bin-hash>]<EXE_SUFFIX>
    let Some(profile) = test.parent().and_then(Path::parent) else {
        return LayoutCandidates::Relocated;
    };
    let fingerprints = profile.join(".fingerprint");
    let test_fingerprint = fingerprints
        .join(format!("{}-{hash}", env!("CARGO_PKG_NAME")))
        .join(&test_fingerprint_name);
    let Ok(bytes) = std::fs::read(&test_fingerprint) else {
        return LayoutCandidates::Relocated;
    };
    let test_fp: serde_json::Value =
        serde_json::from_slice(&bytes).expect("test build fingerprint");
    let dependency = library(&test_fp).expect("test links magi library");
    let mut entries: Vec<_> = std::fs::read_dir(&fingerprints)
        .expect("Cargo fingerprints")
        .map(|entry| entry.expect("Cargo fingerprint entry"))
        .collect();
    entries.sort_by_key(|entry| entry.file_name());

    let mut candidates = Vec::new();
    for entry in entries {
        let Ok(bytes) = std::fs::read(entry.path().join("bin-magi.json")) else {
            continue;
        };
        let Ok(binary) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if library(&binary).as_ref() != Some(&dependency) || binary["rustc"] != test_fp["rustc"] {
            continue;
        }
        let directory = entry.file_name();
        let Some(bin_hash) = directory
            .to_str()
            .and_then(|name| name.rsplit_once('-').map(|(_, hash)| hash))
        else {
            continue;
        };
        let name = if hashed_executable {
            format!("magi-{bin_hash}{}", std::env::consts::EXE_SUFFIX)
        } else {
            format!("magi{}", std::env::consts::EXE_SUFFIX)
        };
        let executable = profile.join("deps").join(name);
        if executable.is_file() && !candidates.contains(&executable) {
            candidates.push(executable);
        }
    }
    LayoutCandidates::Local {
        profile: profile.to_path_buf(),
        candidates,
    }
}

/// Resolve candidate CLI artifacts for `test`, copy each candidate to a
/// content-addressed immutable location, and verify the pinned copy's embedded
/// build identity matches `expected_build_id`. Fails closed on any mismatch.
pub(super) fn resolve_and_pin_cli(
    test: &Path,
    hashed_executable: bool,
    fallback: &Path,
    pin_root_override: Option<&Path>,
    expected_build_id: &str,
) -> Result<PathBuf, String> {
    let (pin_root, candidates, layout_label) = match resolve_layout_candidates(
        test,
        hashed_executable,
    ) {
        LayoutCandidates::Local {
            profile,
            candidates,
        } => {
            if candidates.is_empty() {
                return Err(
                    "no CLI artifact links this test's magi library; rebuild the magi binary"
                        .to_owned(),
                );
            }
            let root = select_pin_root(pin_root_override, Some(&profile))?;
            (root, candidates, "Cargo build layout")
        }
        LayoutCandidates::Relocated => {
            if !fallback.is_file() {
                return Err(format!(
                    "relocated CLI fallback {} does not exist (expected build identity {expected_build_id})",
                    fallback.display()
                ));
            }
            let root = select_pin_root(pin_root_override, None)?;
            (root, vec![fallback.to_path_buf()], "relocated CLI fallback")
        }
    };

    let mut failures = Vec::new();
    for candidate in candidates {
        match pin_executable(&candidate, &pin_root) {
            Ok(pinned) => match probe_build_id(&pinned) {
                Ok(actual) if actual == expected_build_id => return Ok(pinned),
                Ok(actual) => failures.push(format!(
                    "candidate {} (pinned at {}): expected build identity {expected_build_id}, got {actual}",
                    candidate.display(),
                    pinned.display()
                )),
                Err(probe_err) => failures.push(format!(
                    "candidate {} (pinned at {}): expected build identity {expected_build_id}, probe failed: {probe_err}",
                    candidate.display(),
                    pinned.display()
                )),
            },
            Err(pin_err) => failures.push(format!(
                "candidate {}: failed to pin immutable copy: {pin_err}",
                candidate.display()
            )),
        }
    }

    Err(format!(
        "CLI build-identity verification failed ({layout_label}; expected {expected_build_id}): {}",
        failures.join("; ")
    ))
}

fn select_pin_root(
    override_root: Option<&Path>,
    profile: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(root) = override_root {
        std::fs::create_dir_all(root)
            .map_err(|e| format!("create pin cache {}: {e}", root.display()))?;
        return Ok(root.to_path_buf());
    }
    if let Some(profile) = profile {
        let local = profile.join(PIN_DIR_NAME);
        if std::fs::create_dir_all(&local).is_ok() {
            return Ok(local);
        }
    }
    let fallback = std::env::temp_dir().join(PIN_DIR_NAME);
    std::fs::create_dir_all(&fallback)
        .map_err(|e| format!("create fallback pin cache {}: {e}", fallback.display()))?;
    Ok(fallback)
}

/// Copy `source` into `<pin_root>/<sha256>/magi[.exe]`, reusing an existing
/// identical copy atomically and safely under concurrent test processes.
pub(super) fn pin_executable(source: &Path, pin_root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(pin_root)
        .map_err(|e| format!("create pin directory {}: {e}", pin_root.display()))?;
    let bin_name = format!("magi{}", std::env::consts::EXE_SUFFIX);

    let (source_digest, source_len) = hash_file(source)?;
    let existing_dir = pin_root.join(&source_digest);
    let existing_exe = existing_dir.join(&bin_name);
    if existing_exe.is_file() {
        let existing_len = std::fs::metadata(&existing_exe)
            .map_err(|e| format!("stat pinned executable {}: {e}", existing_exe.display()))?
            .len();
        if existing_len != source_len {
            return Err(format!(
                "pinned executable {} length mismatch ({existing_len} != {source_len})",
                existing_exe.display()
            ));
        }
        return Ok(existing_exe);
    }

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
    let staging_dir = pin_root.join(format!(".staging-{}-{seq}-{nanos}", std::process::id()));
    std::fs::create_dir(&staging_dir)
        .map_err(|e| format!("create staging directory {}: {e}", staging_dir.display()))?;

    let copy_result = (|| -> Result<(String, u64), String> {
        let mut input = File::open(source)
            .map_err(|e| format!("open candidate executable {}: {e}", source.display()))?;
        let permissions = input
            .metadata()
            .map_err(|e| format!("stat candidate executable {}: {e}", source.display()))?
            .permissions();
        let staged_exe = staging_dir.join(&bin_name);
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged_exe)
            .map_err(|e| format!("create staged executable {}: {e}", staged_exe.display()))?;

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut copied_len = 0u64;
        loop {
            let n = input
                .read(&mut buffer)
                .map_err(|e| format!("read candidate executable {}: {e}", source.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            output
                .write_all(&buffer[..n])
                .map_err(|e| format!("write staged executable {}: {e}", staged_exe.display()))?;
            copied_len += n as u64;
        }
        #[cfg(unix)]
        {
            // Marking the content-addressed copy read-only on Unix blocks
            // accidental writes while still allowing unlink/prune via the
            // parent directory. On Windows, the newly created file stays
            // writable so `cargo clean`, `disk::prune_dir`, and `TempDir::drop`
            // can reclaim stale copies without leaking binaries.
            let mut pinned_perms = permissions;
            pinned_perms.set_readonly(true);
            output
                .set_permissions(pinned_perms)
                .map_err(|e| format!("set permissions on {}: {e}", staged_exe.display()))?;
        }
        #[cfg(not(unix))]
        let _ = permissions;
        output
            .sync_all()
            .map_err(|e| format!("sync staged executable {}: {e}", staged_exe.display()))?;
        drop(output);
        Ok((hex_digest(hasher), copied_len))
    })();

    let (copied_digest, copied_len) = match copy_result {
        Ok(result) => result,
        Err(err) => {
            remove_tree_best_effort(&staging_dir);
            return Err(err);
        }
    };

    let final_dir = pin_root.join(&copied_digest);
    let final_exe = final_dir.join(&bin_name);
    match std::fs::rename(&staging_dir, &final_dir) {
        // Fixtures retain these paths for their entire process lifetime, including
        // gaps between child launches. Only external cache maintenance may prune them.
        Ok(()) => Ok(final_exe),
        Err(rename_err) => {
            remove_tree_best_effort(&staging_dir);
            if final_exe.is_file() {
                let existing_len = std::fs::metadata(&final_exe)
                    .map_err(|e| format!("stat concurrently pinned {}: {e}", final_exe.display()))?
                    .len();
                if existing_len != copied_len {
                    return Err(format!(
                        "concurrently pinned executable {} length mismatch ({existing_len} != {copied_len})",
                        final_exe.display()
                    ));
                }
                return Ok(final_exe);
            }
            Err(format!(
                "publish pinned executable {} -> {}: {rename_err}",
                staging_dir.display(),
                final_dir.display()
            ))
        }
    }
}

pub(super) fn probe_build_id(executable: &Path) -> Result<String, String> {
    let output = std::process::Command::new(executable)
        .arg(magi::build_id::PROBE_ARG)
        .env_remove(magi::build_id::EXPECT_ENV)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            format!(
                "spawn {} {}: {e}",
                executable.display(),
                magi::build_id::PROBE_ARG
            )
        })?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "exited with {} (stdout: {:?}, stderr: {:?})",
            output.status,
            stdout.trim(),
            stderr.trim()
        ));
    }
    let id = String::from_utf8(output.stdout)
        .map_err(|e| format!("non-UTF-8 probe output: {e}"))?
        .trim()
        .to_owned();
    if id.is_empty() {
        return Err("empty build identity output".to_owned());
    }
    Ok(id)
}

fn hash_file(path: &Path) -> Result<(String, u64), String> {
    let mut file =
        File::open(path).map_err(|e| format!("open {} for hashing: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut total = 0u64;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("read {} for hashing: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total += n as u64;
    }
    Ok((hex_digest(hasher), total))
}

fn hex_digest(hasher: Sha256) -> String {
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn remove_tree_best_effort(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// Start the real CLI in a caller-owned temporary home. Keep that directory
/// alive until the child exits. Callers may override the cwd and environment.
pub fn magi_cli(home: &Path) -> std::process::Command {
    let executable = magi_executable();
    let host_home = home.join("host-home");
    std::fs::create_dir_all(&host_home).expect("create isolated CLI home");
    let mut command = std::process::Command::new(executable);
    command
        .current_dir(home)
        .env("MAGI_HOME", home)
        .env("MAGI_CONFIG_DIR", home.join("config"))
        .env("HOME", &host_home)
        .env("USERPROFILE", &host_home)
        .env("MAGI_NO_AUTOUPDATE", "1")
        .env(magi::build_id::EXPECT_ENV, magi::build_id::BUILD_ID);
    for key in SEAT_ENV.into_iter().chain(WORKER_ENV).chain(CONTROL_ENV) {
        command.env_remove(key);
    }
    // A fresh CLI belongs to this fixture, not the web server that launched
    // the test. In particular, a retained restart port overrides `--port 0`
    // and makes independent web tests contend for the operator's listener.
    // Tests of an actual successor may explicitly set these after this call.
    command
        .env_remove("MAGI_WEB_RESTART_PORT")
        .env_remove("MAGI_WEB_RESUME_LOOP");
    command
}

/// Pass the CLI's isolation settings through an agent launcher or shell mock.
/// The launcher supplies the fixture cwd and fresh seat metadata. Copy only
/// positive defaults here so nested calls retain the identity it just assigned.
pub fn magi_cli_env(home: &Path, binary_key: &str) -> BTreeMap<String, String> {
    let command = magi_cli(home);
    let mut env: BTreeMap<_, _> = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| (key.to_str().unwrap().into(), value.to_str().unwrap().into()))
        })
        .collect();
    env.insert(
        binary_key.into(),
        command.get_program().to_str().unwrap().into(),
    );
    env
}

/// Copy isolation defaults to an outer shell or sandbox command. Apply any
/// test-specific cwd and environment overrides after this call.
pub fn magi_cli_environment(cli: &std::process::Command, outer: &mut std::process::Command) {
    outer.current_dir(cli.get_current_dir().expect("isolated CLI cwd"));
    for (key, value) in cli.get_envs() {
        match value {
            Some(value) => {
                outer.env(key, value);
            }
            None => {
                outer.env_remove(key);
            }
        }
    }
}
