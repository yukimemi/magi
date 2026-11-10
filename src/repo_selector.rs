//! Repository selectors shared by task filing and reassignment.
use crate::{config::Config, repos};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Resolve a repository using task filing selectors and acquisition policy.
pub async fn resolve_repo(repo: &Path, cwd: &Path, hint: Option<&str>) -> Result<PathBuf> {
    resolve_repo_with_clone(repo, cwd, hint, true).await
}

/// Resolve a selector with an explicit acquisition policy.
pub async fn resolve_repo_with_clone(
    repo: &Path,
    cwd: &Path,
    hint: Option<&str>,
    allow_clone: bool,
) -> Result<PathBuf> {
    resolve_repo_with_gh(repo, cwd, hint, allow_clone, "gh").await
}

/// Resolve a selector with a specified forge executable.
pub async fn resolve_repo_with_gh(
    repo: &Path,
    cwd: &Path,
    hint: Option<&str>,
    allow_clone: bool,
    gh: &str,
) -> Result<PathBuf> {
    let candidate = if repo.is_absolute() {
        repo.to_path_buf()
    } else {
        cwd.join(repo)
    };
    match candidate.canonicalize() {
        Ok(canonical) => {
            let top = match crate::git::toplevel(&canonical).await {
                Ok(top) => top,
                Err(e) => {
                    if repo == Path::new(".") {
                        if let Some(home) = dirs::home_dir() {
                            if let Some(found) = repos::discover_verified(
                                &home,
                                &[],
                                hint,
                                crate::updater::repo_name(),
                            )
                            .await
                            {
                                eprintln!(
                                    "the default --repo `.` ({}) is not a git checkout; using {} \
                                 instead - {}",
                                    canonical.display(),
                                    found.path.display(),
                                    found.reason,
                                );
                                return Ok(found.path);
                            }
                        }
                    }
                    return Err(e).with_context(|| {
                        format!("--repo {} is not a git working tree", canonical.display())
                    });
                }
            };
            anyhow::ensure!(
                !crate::repo_clone::acquisition_incomplete(&canonical)
                    && !crate::repo_clone::acquisition_incomplete(&top),
                "--repo {} has an incomplete acquisition; inspect the checkout and its acquisition marker before using it",
                canonical.display()
            );
            Ok(canonical)
        }
        Err(_) => {
            // `[repos] roots` is a machine fact, most often declared in the
            // machine config layer - see `repos_cmd`, which reads it the same
            // way for the same reason. A repository's own `magi.toml` may add
            // to it too (`Config::refuse_split_arrays`'s append policy), which
            // is what lets a test supply roots without touching the machine
            // layer at all.
            let (cfg, _) = Config::discover(cwd, None)
                .with_context(|| format!("--repo {} does not exist", repo.display()))?;
            match local_repo_by_name(repo, &cfg.repos.roots) {
                Lookup::Found(path) => Ok(path),
                Lookup::Ambiguous(error) => Err(error),
                Lookup::Missing(error) => {
                    if allow_clone && cfg.repos.auto_clone {
                        let query = repo.to_string_lossy();
                        if query.contains('/') && !repo.is_absolute() {
                            crate::repo_clone::validate_github_name(&query)?;
                            let _lock = crate::repo_clone::clone_lock(&cfg.repos).await?;
                            match local_repo_by_name(repo, &cfg.repos.roots) {
                                Lookup::Found(path) => return Ok(path),
                                Lookup::Ambiguous(error) => return Err(error),
                                Lookup::Missing(_) => {}
                            }
                            return crate::repo_clone::clone_github_with_gh(&query, &cfg.repos, gh)
                                .await;
                        }
                    }
                    Err(error)
                }
            }
        }
    }
}

/// Only a true local miss permits acquisition; ambiguity remains an error.
enum Lookup {
    Found(PathBuf),
    Missing(anyhow::Error),
    Ambiguous(anyhow::Error),
}

fn local_repo_by_name(repo: &Path, roots: &[PathBuf]) -> Lookup {
    let query = repo.to_string_lossy().replace('\\', "/");
    let hits: Vec<repos::Repo> = repos::scan(roots)
        .into_iter()
        .filter(|r| r.name == query || r.name.rsplit('/').next() == Some(query.as_str()))
        .collect();
    match hits.len() {
        1 => Lookup::Found(hits.into_iter().next().expect("exactly one hit").path),
        0 => Lookup::Missing(anyhow::anyhow!(
            "--repo {} does not exist, and no checkout named `{query}` was found under [repos] roots",
            repo.display()
        )),
        n => Lookup::Ambiguous(anyhow::anyhow!(
            "--repo `{query}` matches {n} checkouts under [repos] roots ({}); use a full path \
             or the exact owner/repo name to disambiguate",
            hits.iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Resolve a local short name, refusing ambiguity.
pub fn resolve_repo_by_name(repo: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    match local_repo_by_name(repo, roots) {
        Lookup::Found(path) => Ok(path),
        Lookup::Missing(error) | Lookup::Ambiguous(error) => Err(error),
    }
}
