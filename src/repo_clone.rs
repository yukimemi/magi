//! Opt-in acquisition after the CLI repository resolver establishes a local miss.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Validate an explicit GitHub owner/repository before invoking external tools.
pub fn validate_github_name(repo: &str) -> anyhow::Result<(&str, &str)> {
    let (owner, name) = repo.split_once('/').unwrap_or(("", ""));
    anyhow::ensure!(
        !owner.is_empty()
            && owner.len() <= 39
            && owner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && !owner.starts_with('-')
            && !owner.ends_with('-')
            && !owner.contains("--")
            && !name.is_empty()
            && name.len() <= 100
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
            && name != "."
            && name != "..",
        "invalid GitHub owner/repo name: {repo:?}"
    );
    Ok((owner, name))
}

/// Process lock released even if a concurrently forked child retains the descriptor.
pub struct CloneLock(std::fs::File);

impl Drop for CloneLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: this guard owns a live file descriptor.
            unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
        let _ = &self.0;
    }
}

/// Serialize acquisitions across CLI processes; callers re-scan under this lock.
pub async fn clone_lock(cfg: &crate::config::Repos) -> anyhow::Result<CloneLock> {
    use anyhow::Context;
    let root = cfg
        .roots
        .first()
        .context("[repos] auto_clone requires a nonempty roots array")?;
    std::fs::create_dir_all(root)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".magi-clone.lock"))?;
    loop {
        match crate::pr_head::try_lock_file(&file) {
            Ok(()) => return Ok(CloneLock(file)),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || cfg!(windows) && e.raw_os_error() == Some(33) =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// A sibling file survives failed/cancelled acquisitions without pre-creating
/// the destination (some configured clone tools require it to be absent).
fn incomplete_marker(checkout: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".magi-clone@");
    name.push(checkout.file_name().unwrap_or_default());
    checkout.with_file_name(name)
}

/// Discovery must not expose a checkout until its acquisition has succeeded.
/// Treat unreadable markers as incomplete rather than publishing uncertain state.
pub fn acquisition_incomplete(checkout: &Path) -> bool {
    !matches!(std::fs::symlink_metadata(incomplete_marker(checkout)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Acquire a confirmed GitHub repository at the configured layout destination.
/// Called only after a local miss under the resolver's acquisition lock.
/// `gh` is the executable path, allowing tests to avoid changing PATH.
pub async fn clone_github_with_gh(
    repo: &str,
    cfg: &crate::config::Repos,
    gh: &str,
) -> anyhow::Result<PathBuf> {
    use anyhow::{Context, ensure};
    let (owner, name) = validate_github_name(repo)?;
    let root = cfg
        .roots
        .first()
        .context("[repos] auto_clone requires a nonempty roots array")?;
    let dest = root.join("github.com").join(owner).join(name);
    match std::fs::symlink_metadata(&dest) {
        Ok(_) => anyhow::bail!("clone destination already exists: {}", dest.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| format!("inspect clone destination {}", dest.display()));
        }
    }
    let view = vec![
        gh.to_owned(),
        "repo".into(),
        "view".into(),
        format!("https://github.com/{repo}"),
        "--json".into(),
        "nameWithOwner".into(),
    ];
    let output = acquisition_command(&view).await?;
    let value: serde_json::Value = serde_json::from_slice(&output).with_context(|| {
        format!(
            "command {view:?} returned invalid JSON: {}",
            String::from_utf8_lossy(&output)
        )
    })?;
    let confirmed = value["nameWithOwner"].as_str().with_context(|| {
        format!(
            "command {view:?} omitted nameWithOwner: {}",
            String::from_utf8_lossy(&output)
        )
    })?;
    validate_github_name(confirmed).with_context(|| {
        format!(
            "command {view:?} returned invalid name: {}",
            String::from_utf8_lossy(&output)
        )
    })?;
    ensure!(
        confirmed.eq_ignore_ascii_case(repo),
        "command {view:?} resolved to {confirmed:?}, refusing a different repository"
    );
    let default = vec![
        gh.to_owned(),
        "repo".into(),
        "clone".into(),
        "https://github.com/{repo}.git".into(),
        "{dest}".into(),
    ];
    let template = cfg.clone_command.as_ref().unwrap_or(&default);
    ensure!(
        !template.is_empty() && !template[0].is_empty(),
        "[repos] clone_command must contain an executable"
    );
    // Validated names cannot contain placeholders. Substitute dest last so its
    // path is never interpreted as another template.
    let argv: Vec<String> = template
        .iter()
        .map(|arg| {
            arg.replace("{repo}", repo)
                .replace("{owner}", owner)
                .replace("{name}", name)
                .replace("{dest}", &dest.to_string_lossy())
        })
        .collect();
    std::fs::create_dir_all(dest.parent().context("clone destination has no parent")?)?;
    let marker = incomplete_marker(&dest);
    std::fs::OpenOptions::new().write(true).create_new(true).open(&marker)
        .with_context(|| format!("cannot mark acquisition at {}; inspect any previous failed acquisition and its marker {}", dest.display(), marker.display()))?;
    // Cancellation/process exit must keep the marker: the child may still run.
    // A completed failure can safely clear it only when no destination exists.
    let acquired: anyhow::Result<PathBuf> = async {
        acquisition_command(&argv).await?;
        let canonical = dest
            .canonicalize()
            .with_context(|| format!("command {argv:?} did not create {}", dest.display()))?;
        let top = crate::git::toplevel(&canonical).await.with_context(|| {
            format!(
                "command {argv:?} did not create a checkout at {}",
                dest.display()
            )
        })?;
        ensure!(
            top.canonicalize()? == canonical && canonical.join(".git").exists(),
            "command {argv:?} did not create a checkout at {}",
            dest.display()
        );
        Ok(canonical)
    }
    .await;
    let canonical = acquired.map_err(|error| {
        if matches!(std::fs::symlink_metadata(&dest),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            if let Err(cleanup) = std::fs::remove_file(&marker) {
                return error.context(format!(
                    "remove acquisition marker {} after failure: {cleanup}",
                    marker.display()
                ));
            }
        }
        error
    })?;
    std::fs::remove_file(&marker).with_context(|| {
        format!(
            "publish completed acquisition by removing {}",
            marker.display()
        )
    })?;
    Ok(canonical)
}

async fn acquisition_command(argv: &[String]) -> anyhow::Result<Vec<u8>> {
    use anyhow::{Context, ensure};
    let output = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .await
        .with_context(|| format!("could not execute command {argv:?}"))?;
    ensure!(
        output.status.success(),
        "command {argv:?} failed ({}):\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

#[cfg(all(test, unix))]
mod acquisition_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn github_names_refuse_traversal_options_and_invalid_characters() {
        for name in [
            "bare",
            "a/b/c",
            "a/..",
            "a/.",
            "a/",
            "/b",
            "a--b/c",
            "a-/b",
            "-a/b",
            "a/b;touch",
            "a/b\\c",
            "a/é",
        ] {
            assert!(validate_github_name(name).is_err(), "{name}");
        }
        assert!(validate_github_name(&format!("{}/b", "a".repeat(40))).is_err());
        assert!(validate_github_name(&format!("a/{}", "b".repeat(101))).is_err());
        for name in ["owner/repo", "a-b/.repo_name-1", "Owner/Repo.git"] {
            validate_github_name(name).unwrap();
        }
    }

    #[tokio::test]
    async fn acquisition_failures_are_explicit_and_preserve_existing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Repos {
            roots: vec![tmp.path().join("root")],
            ..Default::default()
        };
        let gh = script(tmp.path(), "gh", "echo denied; echo forbidden >&2; exit 7");
        let error = clone_github_with_gh("sample/project", &cfg, &gh)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&gh)
                && error.contains("denied")
                && error.contains("forbidden")
                && error.contains('7')
        );
        let gh = script(
            tmp.path(),
            "gh",
            "echo '{\"nameWithOwner\":\"other/project\"}'",
        );
        assert!(
            clone_github_with_gh("sample/project", &cfg, &gh)
                .await
                .unwrap_err()
                .to_string()
                .contains("different repository")
        );
        let gh = script(
            tmp.path(),
            "gh",
            "echo '{\"nameWithOwner\":\"sample/project\"}'",
        );
        let clone = script(tmp.path(), "clone", "echo clone-failed >&2; exit 9");
        cfg.clone_command = Some(vec![clone.clone()]);
        let error = clone_github_with_gh("sample/project", &cfg, &gh)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(&clone) && error.contains("clone-failed"));
        assert!(!acquisition_incomplete(
            &cfg.roots[0].join("github.com/sample/project")
        ));
        let clone = script(tmp.path(), "clone", "exit 0");
        cfg.clone_command = Some(vec![clone]);
        assert!(
            clone_github_with_gh("sample/project", &cfg, &gh)
                .await
                .unwrap_err()
                .to_string()
                .contains("did not create")
        );
        let dest = cfg.roots[0].join("github.com/sample/project");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("keep"), "untouched").unwrap();
        assert!(
            clone_github_with_gh("sample/project", &cfg, &gh)
                .await
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("keep")).unwrap(),
            "untouched"
        );
        cfg.roots.clear();
        assert!(
            clone_github_with_gh("sample/project", &cfg, &gh)
                .await
                .unwrap_err()
                .to_string()
                .contains("nonempty roots")
        );
    }

    #[tokio::test]
    async fn acquisition_retries_after_failure_without_a_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = script(
            tmp.path(),
            "gh",
            "echo '{\"nameWithOwner\":\"sample/project\"}'",
        );
        let mut cfg = crate::config::Repos {
            roots: vec![tmp.path().join("root")],
            clone_command: Some(vec![
                tmp.path().join("missing-command").display().to_string(),
            ]),
            ..Default::default()
        };
        let dest = cfg.roots[0].join("github.com/sample/project");
        assert!(
            clone_github_with_gh("sample/project", &cfg, &gh)
                .await
                .is_err()
        );
        assert!(!dest.exists());
        assert!(!acquisition_incomplete(&dest));
        let clone = script(
            tmp.path(),
            "clone",
            r#"if [ ! -f "$0.attempted" ]; then
    touch "$0.attempted"
    echo transient-failure >&2
    exit 9
fi
git init -q "$1""#,
        );
        cfg.clone_command = Some(vec![clone, "{dest}".into()]);
        let error = clone_github_with_gh("sample/project", &cfg, &gh)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("transient-failure"));
        assert!(!dest.exists());
        assert!(!acquisition_incomplete(&dest));
        let acquired = clone_github_with_gh("sample/project", &cfg, &gh)
            .await
            .unwrap();
        assert_eq!(acquired, dest.canonicalize().unwrap());
        assert!(!acquisition_incomplete(&dest));

        // A failed command that did create a checkout must remain quarantined.
        cfg.roots = vec![tmp.path().join("partial")];
        let partial = cfg.roots[0].join("github.com/sample/project");
        let clone = script(tmp.path(), "partial-clone", "git init -q \"$1\"; exit 9");
        cfg.clone_command = Some(vec![clone, "{dest}".into()]);
        assert!(
            clone_github_with_gh("sample/project", &cfg, &gh)
                .await
                .is_err()
        );
        assert!(partial.join(".git").exists());
        assert!(acquisition_incomplete(&partial));
        assert!(crate::repos::scan(&cfg.roots).is_empty());
        assert!(
            clone_github_with_gh("sample/project", &cfg, &gh)
                .await
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
    }

    #[tokio::test]
    async fn acquisition_default_command_uses_the_exact_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = script(
            tmp.path(),
            "gh",
            r#"export GH_HOST=enterprise.invalid
if [ "$2" = view ]; then
    [ "$3" = https://github.com/sample/project ] || exit 8
    echo '{"nameWithOwner":"Sample/Project"}'
else
    [ "$1" = repo ] && [ "$2" = clone ] && [ "$3" = https://github.com/sample/project.git ] || exit 8
    git init -q "$4"
fi"#,
        );
        let cfg = crate::config::Repos {
            roots: vec![tmp.path().join("root with spaces")],
            ..Default::default()
        };
        let path = clone_github_with_gh("sample/project", &cfg, &gh)
            .await
            .unwrap();
        assert_eq!(
            path,
            cfg.roots[0]
                .join("github.com/sample/project")
                .canonicalize()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn configured_clone_preserves_spaces_in_substituted_arguments() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = script(
            tmp.path(),
            "gh",
            "echo '{\"nameWithOwner\":\"sample/project\"}'",
        );
        let clone = script(
            tmp.path(),
            "clone with spaces",
            r#"[ "$#" = 4 ] && [ "$1" = 'sample x' ] && [ "$2" = project ] && [ "$3" = sample/project ] || exit 8
git init -q "$4""#,
        );
        let cfg = crate::config::Repos {
            roots: vec![tmp.path().join("root with spaces")],
            clone_command: Some(vec![
                clone,
                "{owner} x".into(),
                "{name}".into(),
                "{repo}".into(),
                "{dest}".into(),
            ]),
            ..Default::default()
        };
        let path = clone_github_with_gh("sample/project", &cfg, &gh)
            .await
            .unwrap();
        assert_eq!(
            path,
            cfg.roots[0]
                .join("github.com/sample/project")
                .canonicalize()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn clone_lock_is_exclusive_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = crate::config::Repos {
            roots: vec![tmp.path().to_owned()],
            ..Default::default()
        };
        let first = clone_lock(&cfg).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), clone_lock(&cfg))
                .await
                .is_err()
        );
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), clone_lock(&cfg))
            .await
            .unwrap()
            .unwrap();
    }
}
