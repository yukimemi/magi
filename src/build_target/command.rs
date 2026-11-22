//! Command boundary for leased, isolated Cargo outputs.
use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context as _, Result, bail, ensure};
use sha2::{Digest as _, Sha256};

use crate::proc::Quiet as _;

const TARGET: &str = "MAGI_ISOLATED_CARGO_TARGET";
const SHIM: &str = "MAGI_ISOLATED_CARGO_SHIM";
const OUTPUT_ENV: [&str; 3] = [
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "CARGO_BUILD_BUILD_DIR",
];
const CARGO_EXE: &str = if cfg!(windows) { "cargo.exe" } else { "cargo" };

fn basename(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

fn is_posix_shell(name: &str) -> bool {
    matches!(
        name,
        "sh" | "bash" | "dash" | "sh.exe" | "bash.exe" | "dash.exe"
    )
}

pub(crate) fn check_shell(shell: &[String]) -> Result<()> {
    ensure!(
        matches!(shell, [program, flag] if flag == "-c" && is_posix_shell(basename(program))),
        "isolated build verification requires a POSIX shell configured as sh -c, bash -c, or dash -c"
    );
    Ok(())
}

struct Word<'a> {
    source: &'a str,
    text: Cow<'a, str>,
    expands: bool,
}

// Recognize words and stop at a supported command boundary without rewriting
// shell text. Anything that can expand into a command is outside this grammar.
fn word<'a>(rest: &mut &'a str) -> Result<Option<Word<'a>>> {
    *rest = rest.trim_start_matches([' ', '\t']);
    let input = *rest;
    if input.is_empty() || input.starts_with("&&") {
        return Ok(None);
    }
    let mut quote = None;
    let mut copy = None::<String>;
    let mut expands = false;
    let mut chars = input.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        match (quote, ch) {
            (None, c) if matches!(c, ' ' | '\t') || (c == '&' && input[i..].starts_with("&&")) => {
                *rest = &input[i..];
                let source = &input[..i];
                return Ok(Some(Word {
                    source,
                    text: copy.map_or(Cow::Borrowed(source), Cow::Owned),
                    expands,
                }));
            }
            (None, '\n' | '\r' | ';' | '&' | '|' | '(' | ')' | '<' | '>') => {
                bail!(
                    "isolated build verification supports only simple commands and Cargo `&&` chains without redirections"
                )
            }
            (None | Some('"'), '`') => {
                bail!("isolated build verification does not support command substitutions")
            }
            (None | Some('"'), '$') => {
                ensure!(
                    !chars.peek().is_some_and(|&(_, c)| c == '('),
                    "isolated build verification does not support command substitutions"
                );
                expands = true;
            }
            (None, '*' | '?' | '[' | '{' | '~' | '#' | '!') => expands = true,
            (None, '\'' | '"') => {
                quote = Some(ch);
                copy.get_or_insert_with(|| input[..i].to_owned());
                continue;
            }
            (Some(q), c) if q == c => {
                quote = None;
                continue;
            }
            (None | Some('"'), '\\') => {
                let next = chars.peek().copied();
                if quote.is_none()
                    || next.is_some_and(|(_, c)| matches!(c, '"' | '$' | '`' | '\\' | '\n'))
                {
                    let (_, c) = chars
                        .next()
                        .context("unfinished shell escape in isolated build command")?;
                    ensure!(
                        c != '\n' && c != '\r',
                        "isolated build verification does not support shell line continuations"
                    );
                    copy.get_or_insert_with(|| input[..i].to_owned()).push(c);
                    continue;
                }
            }
            _ => {}
        }
        if let Some(copy) = &mut copy {
            copy.push(ch);
        }
    }
    ensure!(
        quote.is_none(),
        "unclosed shell quote in isolated build command"
    );
    *rest = "";
    Ok(Some(Word {
        source: input,
        text: copy.map_or(Cow::Borrowed(input), Cow::Owned),
        expands,
    }))
}

fn assignment(value: &str) -> Option<&str> {
    let (key, _) = value.split_once('=')?;
    let first = key.as_bytes().first()?;
    (matches!(first, b'a'..=b'z' | b'A'..=b'Z' | b'_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    .then_some(key)
}

fn protected(key: &str) -> bool {
    OUTPUT_ENV
        .iter()
        .copied()
        .chain([TARGET, SHIM, "CARGO", "PATH", "BASH_ENV", "ENV"])
        .any(|k| key.eq_ignore_ascii_case(k))
}

fn validate_override(arg: &OsStr) -> Result<()> {
    let arg = arg.as_encoded_bytes();
    let flag = arg.split(|&b| b == b'=').next().unwrap_or(arg);
    ensure!(
        !matches!(
            flag,
            b"--target-dir"
                | b"--build-dir"
                | b"--config"
                | b"--artifact-dir"
                | b"--out-dir"
                | b"--export-dir"
                | b"--archive-file"
                | b"--extract-to"
                | b"--binaries-metadata"
                | b"--cargo-metadata"
                | b"--target-dir-remap"
                | b"--env-file"
        ) && !arg.starts_with(b"-Z")
            && !arg.starts_with(b"-C"),
        "Cargo argument `{}` is not supported with isolated build outputs",
        String::from_utf8_lossy(flag)
    );
    let value = arg
        .strip_prefix(b"--env=")
        .or_else(|| arg.strip_prefix(b"-e="))
        .unwrap_or(arg);
    if let Some(index) = value.iter().position(|&b| b == b'=')
        && let Ok(key) = std::str::from_utf8(&value[..index])
    {
        ensure!(
            !protected(key),
            "Cargo environment override `{key}` is not supported with isolated build outputs"
        );
    }
    Ok(())
}

/// Remove only a literal leading CARGO_TARGET_DIR assignment. The rest is
/// still the operator's shell text, including its original quoting.
pub(crate) fn normalize(mut command: &str) -> Result<&str> {
    let mut normalized = command;
    let starts_with_cargo = normalize_clause(&mut command, Some(&mut normalized))?;
    if !command.is_empty() {
        ensure!(
            starts_with_cargo,
            "isolated `&&` chains must invoke Cargo by name in every clause"
        );
    }
    while !command.is_empty() {
        command = command
            .strip_prefix("&&")
            .context("unsupported isolated build command separator")?;
        ensure!(
            normalize_clause(&mut command, None)?,
            "isolated `&&` chains must invoke Cargo by name in every clause"
        );
    }
    Ok(normalized)
}

fn normalize_clause<'a>(rest: &mut &'a str, mut prefix: Option<&mut &'a str>) -> Result<bool> {
    let program = loop {
        let token = word(rest)?.context("isolated build command has no program")?;
        let leading = prefix.take();
        if token.source.starts_with("CARGO_TARGET_DIR=")
            && let Some(out) = leading
        {
            ensure!(
                !token.expands,
                "isolated build verification cannot replace a nonliteral CARGO_TARGET_DIR assignment"
            );
            ensure!(
                token.text.len() > "CARGO_TARGET_DIR=".len(),
                "isolated build verification cannot replace an empty CARGO_TARGET_DIR assignment"
            );
            *out = rest.trim_start_matches([' ', '\t']);
        } else if let Some(key) = assignment(&token.text) {
            ensure!(
                !protected(key),
                "isolated build verification only replaces a literal leading CARGO_TARGET_DIR assignment; unsupported override `{key}`"
            );
            // Quoted or escaped assignment names are not POSIX assignments.
            ensure!(
                token
                    .source
                    .strip_prefix(key)
                    .is_some_and(|s| s.starts_with('=')),
                "ambiguous shell assignment in isolated build verification"
            );
        } else {
            ensure!(
                !token.expands,
                "isolated build verification requires a literal program name"
            );
            break token;
        }
    };
    let name = basename(&program.text);
    let name = match name.rsplit_once('.') {
        Some((stem, suffix))
            if ["exe", "cmd", "bat", "ps1"]
                .iter()
                .any(|ext| suffix.eq_ignore_ascii_case(ext)) =>
        {
            stem
        }
        _ => name,
    };
    ensure!(
        !matches!(
            name,
            "env"
                | "command"
                | "exec"
                | "eval"
                | "source"
                | "."
                | "set"
                | "export"
                | "unset"
                | "alias"
                | "sh"
                | "bash"
                | "dash"
                | "zsh"
                | "fish"
                | "cmd"
                | "pwsh"
                | "powershell"
                | "rustup"
                | "xargs"
                | "find"
        ),
        "shell launcher `{name}` is not supported in isolated build verification"
    );
    let cargo = name.eq_ignore_ascii_case("cargo");
    if cargo {
        ensure!(
            program.text.eq_ignore_ascii_case("cargo")
                || program.text.eq_ignore_ascii_case(CARGO_EXE),
            "isolated build verification requires native Cargo on PATH, not a script or explicit executable path"
        );
    } else {
        ensure!(
            !name
                .get(..6)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("cargo-"))
                && !name.eq_ignore_ascii_case("makers"),
            "invoke Cargo extensions through `cargo` with isolated build outputs"
        );
    }
    let mut cargo_options = cargo;
    while let Some(token) = word(rest)? {
        if cargo_options {
            if token.text == "--" {
                cargo_options = false;
            } else {
                validate_override(OsStr::new(token.text.as_ref()))?;
            }
        }
    }
    Ok(cargo)
}

fn key_matches(key: &OsStr, name: &str) -> bool {
    key == name
        || (cfg!(windows)
            && key
                .to_str()
                .is_some_and(|key| key.eq_ignore_ascii_case(name)))
}

fn value(cmd: &Command, name: &str) -> Option<OsString> {
    if let Some((_, value)) = cmd.get_envs().find(|(key, _)| key_matches(key, name)) {
        value.map(OsStr::to_owned)
    } else {
        env::var_os(name)
    }
}

// Win32 needs the real drive path in PATH so an MSYS shell can import it.
// Ownership checks still use the canonical, possibly verbatim, PathBuf.
fn process_path(path: &Path) -> Result<Cow<'_, Path>> {
    #[cfg(windows)]
    {
        let text = path
            .to_str()
            .context("isolated build path is not Unicode")?;
        if let Some(drive) = text.strip_prefix(r"\\?\") {
            ensure!(
                drive.as_bytes().get(1..3) == Some(b":\\"),
                "isolated build requires a drive-letter path on Windows"
            );
            return Ok(Cow::Borrowed(Path::new(drive)));
        }
    }
    Ok(Cow::Borrowed(path))
}

fn executable_key(path: &Path) -> Result<String> {
    let path = path.canonicalize()?;
    let metadata = fs::metadata(&path)?;
    let mut hash = Sha256::new();
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(
        metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
            .to_le_bytes(),
    );
    const HEX: &[u8] = b"0123456789abcdef";
    let mut key = String::with_capacity(64);
    for &byte in hash.finalize().iter() {
        key.push(char::from(HEX[(byte >> 4) as usize]));
        key.push(char::from(HEX[(byte & 15) as usize]));
    }
    Ok(key)
}

fn publish(dir: &Path, source: &Path, name: &str) -> Result<PathBuf> {
    let destination = dir.join(name);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::hard_link(source, &destination) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => {
                    // A different volume cannot hard link. Publish only a
                    // complete copy; other launchers can race this setup.
                    let temporary =
                        dir.join(crate::rng::SplitMix64::new(crate::rng::entropy()).uuid_v4());
                    let result = fs::copy(source, &temporary)
                        .and_then(|_| fs::hard_link(&temporary, &destination));
                    let _ = fs::remove_file(&temporary);
                    match result {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        Err(error) => return Err(error.into()),
    }
    super::plain(&destination, false)?;
    Ok(destination)
}

fn ensure_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    super::plain(path, true)
}

fn shim_dir(root: &Path, key: &str) -> Result<PathBuf> {
    ensure_dir(root)?;
    let dir = root.join(key);
    ensure_dir(&dir)?;
    Ok(dir)
}

fn setup_shim(target: &Path) -> Result<PathBuf> {
    ensure!(
        target.is_absolute(),
        "isolated Cargo target must be absolute"
    );
    let source = env::current_exe()?.canonicalize()?;
    let root = target
        .parent()
        .and_then(Path::parent)
        .context("isolated target has no namespace")?
        .canonicalize()?
        .join(".cargo-shims");
    let dir = shim_dir(&root, &executable_key(&source)?)?;
    publish(&dir, &source, CARGO_EXE)
}

fn without_shim<'a>(path: &'a OsStr, shim: Option<&'a Path>) -> impl Iterator<Item = PathBuf> + 'a {
    let dir = shim.and_then(Path::parent);
    env::split_paths(path).filter(move |path| {
        !dir.is_some_and(|dir| {
            path == dir
                || (cfg!(windows)
                    && path
                        .to_str()
                        .zip(dir.to_str())
                        .is_some_and(|(path, dir)| path.eq_ignore_ascii_case(dir)))
        })
    })
}

/// PATH for independent test fixtures that explicitly own their Cargo outputs.
#[doc(hidden)]
pub fn standalone_fixture_path() -> Result<OsString> {
    let shim = env::var_os(SHIM);
    let path = env::var_os("PATH").context("fixture has no PATH")?;
    Ok(env::join_paths(without_shim(
        &path,
        shim.as_deref().map(Path::new),
    ))?)
}

pub(crate) fn cargo(target: &Path) -> Result<Command> {
    let shim = setup_shim(target).context("prepare isolated Cargo launcher")?;
    let mut cmd = Command::new(&shim);
    configure_environment(&mut cmd, target, &shim)?;
    Ok(cmd)
}

fn configure_environment(cmd: &mut Command, target: &Path, shim: &Path) -> Result<()> {
    let shim = process_path(shim)?;
    let target = process_path(target)?;
    if let Some(text) = target.to_str() {
        ensure!(
            ![
                "{workspace-root}",
                "{cargo-cache-home}",
                "{workspace-path-hash}"
            ]
            .iter()
            .any(|variable| text.contains(variable)),
            "isolated build target must not contain Cargo build directory template variables"
        );
    }
    let path = value(cmd, "PATH").unwrap_or_default();
    let old_shim = value(cmd, SHIM);
    let dir = shim
        .parent()
        .context("isolated Cargo launcher has no directory")?;
    let path = env::join_paths(
        std::iter::once(dir.to_path_buf())
            .chain(without_shim(&path, old_shim.as_deref().map(Path::new))),
    )?;
    cmd.env("PATH", path)
        .env("CARGO", shim.as_os_str())
        .env(SHIM, shim.as_os_str())
        .env(TARGET, target.as_os_str());
    for key in OUTPUT_ENV {
        cmd.env(key, target.as_os_str());
    }
    Ok(())
}

pub(crate) fn configure(cmd: &mut tokio::process::Command, target: &Path) -> Result<()> {
    let shim = setup_shim(target).context("prepare isolated Cargo launcher")?;
    let cmd = cmd.as_std_mut();
    if cmd
        .get_program()
        .to_str()
        .is_some_and(|program| is_posix_shell(basename(program)))
    {
        cmd.env_remove("BASH_ENV")
            .env_remove("ENV")
            .env_remove("BASH_FUNC_cargo%%");
    }
    configure_environment(cmd, target, &shim)
}

pub(crate) fn clear(cmd: &mut tokio::process::Command) -> Result<()> {
    clear_std(cmd.as_std_mut())
}

pub(crate) fn clear_std(cmd: &mut Command) -> Result<()> {
    if let Some(shim) = value(cmd, SHIM) {
        if let Some(path) = value(cmd, "PATH") {
            cmd.env(
                "PATH",
                env::join_paths(without_shim(&path, Some(Path::new(&shim))))?,
            );
        }
        if value(cmd, "CARGO").as_deref() == Some(shim.as_os_str()) {
            cmd.env_remove("CARGO");
        }
    }
    if value(cmd, TARGET).is_some() {
        for key in OUTPUT_ENV {
            cmd.env_remove(key);
        }
    }
    cmd.env_remove(SHIM).env_remove(TARGET);
    Ok(())
}

fn installed(name: &str, shim: &Path, plugin: bool) -> Result<PathBuf> {
    let cwd = env::current_dir()?;
    let path = env::var_os("PATH").context("isolated Cargo launcher has no PATH")?;
    let home_bin = if plugin {
        env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|dir| dir.join(".cargo")))
            .map(|dir| cwd.join(dir).join("bin"))
            .filter(|bin| !env::split_paths(&path).any(|dir| cwd.join(dir) == *bin))
    } else {
        None
    };
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    home_bin
        .into_iter()
        .chain(without_shim(&path, Some(shim)))
        .map(|dir| cwd.join(dir).join(&file))
        .find(|path| {
            if path == shim {
                return false;
            }
            fs::metadata(path).is_ok_and(|meta| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    meta.is_file() && meta.permissions().mode() & 0o111 != 0
                }
                #[cfg(not(unix))]
                meta.is_file()
            })
        })
        .with_context(|| format!("`{file}` is not installed on the managed Cargo PATH"))
}

fn subcommand(args: &[OsString]) -> Result<Option<usize>> {
    let toolchain = args
        .first()
        .is_some_and(|arg| arg.as_encoded_bytes().starts_with(b"+"));
    let mut args = args.iter().enumerate().skip(usize::from(toolchain));
    while let Some((i, arg)) = args.next() {
        ensure!(arg != "--", "Cargo option separator needs a subcommand");
        if arg == "--color" || arg == "--explain" {
            args.next();
        } else if !arg.as_encoded_bytes().starts_with(b"-") {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn cargo_command(args: &[OsString], shim: &Path, target: &Path) -> Result<Command> {
    for arg in args.iter().take_while(|arg| *arg != "--") {
        validate_override(arg)?;
    }
    let index = subcommand(args)?;
    let name = index.and_then(|i| args[i].to_str());
    ensure!(
        index.is_none() || name.is_some(),
        "non-Unicode Cargo subcommand is not supported with isolated build outputs"
    );
    let plugin = match name {
        Some("make") => Some("cargo-make"),
        Some("nextest") => Some("cargo-nextest"),
        Some("clippy") => Some("cargo-clippy"),
        Some("fmt") => Some("cargo-fmt"),
        Some("clean") => {
            ensure!(
                args.iter().take_while(|arg| *arg != "--").any(|arg| {
                    arg.as_encoded_bytes().starts_with(b"-p")
                        || arg == "--package"
                        || arg.as_encoded_bytes().starts_with(b"--package=")
                }),
                "isolated Cargo clean requires an explicit package; the target directory must survive"
            );
            None
        }
        None
        | Some(
            "build" | "check" | "test" | "run" | "doc" | "rustc" | "rustdoc" | "bench" | "fix"
            | "metadata" | "tree" | "fetch" | "update" | "generate-lockfile" | "pkgid"
            | "locate-project" | "verify-project" | "read-manifest" | "new" | "init" | "add"
            | "remove" | "help" | "search" | "info",
        ) => None,
        Some(name) => bail!(
            "Cargo subcommand or alias `{name}` is not supported with isolated build outputs; use the full name of a supported Cargo command"
        ),
    };
    let mut cmd = if let (Some(plugin), Some(index)) = (plugin, index) {
        // Cargo replaces CARGO with its own path when it starts extensions.
        // Dispatch these extensions directly so their Cargo calls stay guarded.
        let toolchain = args
            .first()
            .and_then(|arg| arg.to_str())
            .and_then(|arg| arg.strip_prefix('+'));
        ensure!(
            index == usize::from(toolchain.is_some()),
            "Cargo global flags before `{}` are not supported with isolated build outputs; put them after the subcommand",
            args[index].to_string_lossy()
        );
        let program = installed(plugin, shim, true)?;
        #[cfg(windows)]
        let program = if plugin == "cargo-make" {
            stage_cargo_make(shim, &program)?
        } else {
            program
        };
        let mut cmd = Command::new(program);
        if let Some(toolchain) = toolchain {
            cmd.env("RUSTUP_TOOLCHAIN", toolchain);
        }
        cmd.args(&args[index..]);
        cmd
    } else {
        let mut cmd = Command::new(installed("cargo", shim, false)?);
        cmd.args(args);
        cmd
    };
    cmd.env("CARGO", shim);
    for key in OUTPUT_ENV {
        cmd.env(key, target);
    }
    Ok(cmd)
}

#[cfg(windows)]
fn stage_cargo_make(shim: &Path, program: &Path) -> Result<PathBuf> {
    // Windows Rust Command searches the application's directory before the
    // inherited PATH. cargo-make launches task commands by name, so when it
    // lives by rustup's cargo.exe, PATH alone does not intercept those tasks.
    let shim = shim.canonicalize()?;
    let root = shim
        .parent()
        .context("isolated Cargo launcher has no directory")?;
    let program = program.canonicalize()?;
    let dir = shim_dir(root, &executable_key(&program)?)?;
    publish(&dir, &shim, CARGO_EXE)?;
    publish(&dir, &program, "cargo-make.exe")
}

fn launch() -> Result<ExitCode> {
    let shim =
        PathBuf::from(env::var_os(SHIM).context("missing isolated Cargo launcher configuration")?);
    let target = PathBuf::from(env::var_os(TARGET).context("missing isolated Cargo target")?);
    ensure!(
        target.is_absolute() && shim.is_absolute(),
        "isolated Cargo launcher paths must be absolute"
    );
    for key in OUTPUT_ENV {
        ensure!(
            env::var_os(key).as_deref() == Some(target.as_os_str()),
            "isolated Cargo child changed or removed `{key}`; refusing to build outside its leased target"
        );
    }
    let args: Vec<_> = env::args_os().skip(1).collect();
    let mut cmd = cargo_command(&args, &shim, &target)?;
    cmd.quiet();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        Err(cmd.exec()).context("launch isolated Cargo")
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().context("launch isolated Cargo")?;
        Ok(status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .map_or(ExitCode::FAILURE, ExitCode::from))
    }
}

/// Dispatch a native shim before initializing the application or its runtime.
#[doc(hidden)]
pub fn shim() -> Option<ExitCode> {
    let executable = env::current_exe().ok()?;
    if !executable
        .file_name()
        .is_some_and(|name| key_matches(name, CARGO_EXE))
    {
        return None;
    }
    Some(match launch() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("isolated Cargo: {error:#}");
            ExitCode::FAILURE
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_only_the_literal_target_prefix() {
        let cargo = r#"cargo test --manifest-path 'crates/日本 語/Cargo.toml' -- '--target-dir'"#;
        assert_eq!(normalize(cargo).unwrap(), cargo);
        for prefix in [
            r#"CARGO_TARGET_DIR='C:\old cache\日本 語'"#,
            r#"CARGO_TARGET_DIR="/old cache/日本 語""#,
            "CARGO_TARGET_DIR=/tmp/old-cache",
        ] {
            assert_eq!(
                normalize(&format!("  {prefix}\tRUSTUP_TOOLCHAIN=stable {cargo}")).unwrap(),
                format!("RUSTUP_TOOLCHAIN=stable {cargo}")
            );
        }
        assert_eq!(
            normalize("echo CARGO_TARGET_DIR=/message").unwrap(),
            "echo CARGO_TARGET_DIR=/message"
        );
        let chain = r#"cargo check && cargo test -- '--target-dir && hi'"#;
        assert_eq!(
            normalize(&format!("CARGO_TARGET_DIR='old cache' {chain}")).unwrap(),
            chain
        );
    }

    #[test]
    fn rejects_unsupported_shell_and_output_overrides() {
        for command in [
            "CARGO_TARGET_DIR=${HOME}/cache cargo test",
            "CARGO_TARGET_DIR= cargo test",
            "X=1 CARGO_TARGET_DIR=/tmp cargo test",
            "export CARGO_TARGET_DIR=/tmp; cargo test",
            "env CARGO_TARGET_DIR=/tmp cargo test",
            "cargo build & cargo test",
            "cargo build &&",
            "cargo build && CARGO_TARGET_DIR=/tmp cargo test",
            "cargo build && '/opt/rust/bin/cargo' test",
            "cargo build && export PATH=/tmp",
            "hash -p /opt/rust/bin/cargo cargo && cargo test",
            "cargo build --manifest-path $(echo Cargo.toml)",
            "cargo build '--target-dir=/tmp/out'",
            "cargo make --env=CARGO_TARGET_DIR=/tmp/out",
            "cargo make --env-file output.env",
            "CARGO_TARGET_DIR='/old cache' cargo.cmd build",
            "cargo nextest run --config overrides.toml",
            "cargo build --artifact-dir /tmp/out",
            "CARGO_BUILD_BUILD_DIR=/tmp cargo test",
            "'/opt/rust/bin/cargo' build",
            r#"'C:\Tools\cargo.exe' build"#,
        ] {
            assert!(normalize(command).is_err(), "accepted {command}");
        }
    }

    #[test]
    fn attaching_a_new_target_replaces_only_the_previous_attachment() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let root = process_path(&root).unwrap();
        let prior_bin = root.join("prior bin");
        let new_bin = root.join("next bin");
        let tools = root.join("regular tools");
        let old_shim = prior_bin.join(CARGO_EXE);
        let new_shim = new_bin.join(CARGO_EXE);
        let target = root.join("new 日本 語 target");
        let mut cmd = tokio::process::Command::new("agent");
        cmd.env("PATH", env::join_paths([prior_bin, tools.clone()]).unwrap())
            .env(SHIM, &old_shim)
            .env("CARGO", &old_shim)
            .env(TARGET, root.join("prior target"))
            .env("RUSTC_WRAPPER", "compiler wrapper");

        configure_environment(cmd.as_std_mut(), &target, &new_shim).unwrap();
        let child = cmd.as_std();
        assert_eq!(
            env::split_paths(&value(child, "PATH").unwrap()).collect::<Vec<_>>(),
            [new_bin, tools.clone()]
        );
        assert_eq!(value(child, "CARGO").as_deref(), Some(new_shim.as_os_str()));
        assert_eq!(
            value(child, "CARGO_TARGET_DIR").as_deref(),
            Some(target.as_os_str())
        );
        assert_eq!(
            value(child, "RUSTC_WRAPPER").as_deref(),
            Some(OsStr::new("compiler wrapper"))
        );

        clear(&mut cmd).unwrap();
        let child = cmd.as_std();
        assert_eq!(
            env::split_paths(&value(child, "PATH").unwrap()).collect::<Vec<_>>(),
            [tools]
        );
        for key in [TARGET, SHIM, "CARGO"].into_iter().chain(OUTPUT_ENV) {
            assert_eq!(value(child, key), None);
        }
    }

    #[test]
    fn recognizes_the_bundled_posix_shell_on_windows() {
        assert!(
            check_shell(&[
                r"C:\Program Files\Git\bin\sh.exe".to_owned(),
                "-c".to_owned()
            ])
            .is_ok()
        );
        assert!(check_shell(&["powershell".to_owned(), "-Command".to_owned()]).is_err());
    }
}
