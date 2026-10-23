//! Where a repository keeps its release version, and how to rewrite it.
//!
//! A release bump used to read and rewrite the root `Cargo.toml` and nothing
//! else. A Tauri application keeps its manifest under `src-tauri/` and repeats
//! the application version in `package.json` and `tauri.conf.json`, so the bump
//! had to be skipped for it. `[release] version_files` names those files
//! explicitly; nothing is detected, because the repository's own `auto-tag.yml`
//! reads exactly one file and guessing a different one would tag the wrong
//! version.
//!
//! Everything here is a pure function over text, except [`apply`] which writes
//! inside a worktree it is handed.
//!
//! - The first entry is the version of record (`kind = "cargo"`, the file
//!   `auto-tag.yml` reads). The `Cargo.lock` beside it follows, edited as text
//!   rather than through `cargo build`, which a Tauri crate cannot always run.
//! - JSON files are edited by replacing one string, so formatting and key order
//!   survive. `package-lock.json` is deliberately not touched.
//! - An empty list means the root `Cargo.toml`, as before.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::config::{VersionFile, VersionKind};

/// The manifest used when `version_files` is empty.
pub const DEFAULT_PRIMARY: &str = "Cargo.toml";

/// Refuse a malformed `version_files`.
pub fn validate(files: &[VersionFile]) -> Result<()> {
    let Some(first) = files.first() else {
        return Ok(());
    };
    if first.kind != VersionKind::Cargo || !first.path.ends_with("Cargo.toml") {
        bail!(
            "the first entry is the version of record and must be a `Cargo.toml` with kind \
             \"cargo\" (the file auto-tag.yml reads), not `{}`",
            first.path
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for f in files {
        check_path(&f.path)?;
        if f.kind == VersionKind::Json && !f.path.ends_with(".json") {
            bail!("`{}` is kind \"json\" but does not end in .json", f.path);
        }
        if !seen.insert(f.path.as_str()) {
            bail!("`{}` is listed twice", f.path);
        }
    }
    Ok(())
}

/// A path must stay inside the repository: relative, `/`-separated, no `..`.
fn check_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(':')
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        bail!(
            "`{path}` must be a plain relative path inside the repository, like `src-tauri/Cargo.toml`"
        );
    }
    Ok(())
}

/// Were `version_files` configured at all?
pub fn is_custom(files: &[VersionFile]) -> bool {
    !files.is_empty()
}

/// The files a bump rewrites: the configured ones, else the root `Cargo.toml`.
pub fn effective(files: &[VersionFile]) -> Vec<VersionFile> {
    if files.is_empty() {
        vec![VersionFile {
            path: DEFAULT_PRIMARY.to_owned(),
            kind: VersionKind::Cargo,
        }]
    } else {
        files.to_vec()
    }
}

/// The version of record's path.
pub fn primary_path(files: &[VersionFile]) -> &str {
    files.first().map_or(DEFAULT_PRIMARY, |f| f.path.as_str())
}

/// The `Cargo.lock` beside a `Cargo.toml` path.
pub fn lock_path(manifest: &str) -> String {
    match manifest.rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/Cargo.lock"),
        None => "Cargo.lock".to_owned(),
    }
}

/// Did a change touch only the release files and the lockfile? (Its paths, not
/// its content: the caller still checks the version moved.)
pub fn release_only(changed: &[String], files: &[VersionFile]) -> bool {
    let eff = effective(files);
    let lock = lock_path(primary_path(&eff));
    !changed.is_empty()
        && changed
            .iter()
            .all(|c| *c == lock || eff.iter().any(|f| f.path == *c))
}

/// Read the version out of a file of the given kind.
pub fn read_version(kind: VersionKind, text: &str) -> Result<String> {
    match kind {
        VersionKind::Cargo => crate::bump::current_version(text),
        VersionKind::Json => json_version(text)
            .map(|(_, _, v)| v)
            .context("no top-level `\"version\"` string found"),
    }
}

/// Rewrite the version in a file of the given kind.
pub fn rewrite_version(kind: VersionKind, text: &str, new: &str) -> Result<String> {
    match kind {
        VersionKind::Cargo => crate::bump::rewrite_cargo_version(text, new),
        VersionKind::Json => {
            let (start, end, _) =
                json_version(text).context("no top-level `\"version\"` string found")?;
            Ok(format!("{}{new}{}", &text[..start], &text[end..]))
        }
    }
}

/// Find the top-level `"version": "<v>"` of a JSON document: the byte range of
/// the value's content and the value. No full parse - only strings and bracket
/// depth are tracked, so the rest of the document is never reformatted.
fn json_version(text: &str) -> Option<(usize, usize, String)> {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'"' => {
                let (start, end) = string_at(b, i)?;
                i = end + 1;
                if depth == 1 && &text[start..end] == "version" {
                    let mut j = skip_ws(b, i);
                    if b.get(j) == Some(&b':') {
                        j = skip_ws(b, j + 1);
                        if b.get(j) == Some(&b'"') {
                            let (vs, ve) = string_at(b, j)?;
                            return Some((vs, ve, text[vs..ve].to_owned()));
                        }
                    }
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The content range of the string literal opening at `open`.
fn string_at(b: &[u8], open: usize) -> Option<(usize, usize)> {
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Some((open + 1, i)),
            _ => i += 1,
        }
    }
    None
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// The `[package] name` of a manifest.
pub fn package_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
            continue;
        }
        if in_package
            && let Some((k, v)) = t.split_once('=')
            && k.trim() == "name"
        {
            return Some(v.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Move `name`'s entry in a `Cargo.lock` from `old` to `new`, as text. Exactly
/// one entry must match: a lock that does not know the package is a lock the
/// bump cannot vouch for.
pub fn rewrite_lock(lock: &str, name: &str, old: &str, new: &str) -> Result<String> {
    let lines: Vec<&str> = lock.split_inclusive('\n').collect();
    let mut out = String::with_capacity(lock.len());
    let mut block: Vec<&str> = Vec::new();
    let mut hits = 0;
    let mut flush = |block: &mut Vec<&str>, out: &mut String| {
        let named = block
            .iter()
            .any(|l| l.trim() == format!("name = \"{name}\""));
        let at = block
            .iter()
            .position(|l| l.trim() == format!("version = \"{old}\""));
        for (n, l) in block.iter().enumerate() {
            match at {
                Some(a) if named && a == n => {
                    hits += 1;
                    let eol = &l[l.trim_end_matches(['\r', '\n']).len()..];
                    out.push_str(&format!("version = \"{new}\"{eol}"));
                }
                _ => out.push_str(l),
            }
        }
        block.clear();
    };
    for l in lines {
        if l.starts_with('[') {
            flush(&mut block, &mut out);
        }
        block.push(l);
    }
    flush(&mut block, &mut out);
    if hits != 1 {
        bail!("Cargo.lock has {hits} entries for `{name}` at {old}; expected exactly one");
    }
    Ok(out)
}

/// All declared files must agree. `versions` is `(path, version)`.
pub fn agree(versions: &[(String, String)]) -> Result<String> {
    let Some((_, first)) = versions.first() else {
        bail!("no version file");
    };
    if versions.iter().any(|(_, v)| v != first) {
        let list = versions
            .iter()
            .map(|(p, v)| format!("{p} = {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("the version files disagree ({list}); not guessing which is right");
    }
    Ok(first.clone())
}

/// Resolve `rel` inside `root`, refusing a symlink that leaves it.
fn inside(root: &Path, rel: &str) -> Result<PathBuf> {
    check_path(rel)?;
    let path = root.join(rel);
    let real_root = root.canonicalize().context("resolve the worktree")?;
    let real = path
        .canonicalize()
        .with_context(|| format!("resolve {}", path.display()))?;
    if !real.starts_with(&real_root) {
        bail!("`{rel}` resolves outside the repository");
    }
    Ok(path)
}

/// Rewrite every configured file in `worktree` to `new`, and the `Cargo.lock`
/// beside the version of record. Only for configured `version_files`: the
/// default root-manifest path keeps `cargo build` for its lock.
pub fn apply(worktree: &Path, files: &[VersionFile], new: &str) -> Result<()> {
    validate(files)?;
    let primary = files.first().context("no version files configured")?;
    let primary_path_buf = inside(worktree, &primary.path)?;
    let primary_text = std::fs::read_to_string(&primary_path_buf)
        .with_context(|| format!("read {}", primary.path))?;
    let old = read_version(VersionKind::Cargo, &primary_text)?;
    let name = package_name(&primary_text);

    let lock_rel = lock_path(&primary.path);
    let lock = inside(worktree, &lock_rel)
        .ok()
        .and_then(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)));
    let new_lock = match (&lock, &name) {
        (Some((_, text)), Some(n)) => Some(rewrite_lock(text, n, &old, new)?),
        (Some(_), None) => bail!(
            "{} has a Cargo.lock but no `[package] name`; cannot move its entry",
            primary.path
        ),
        _ => None,
    };

    let mut staged = Vec::new();
    for f in files {
        let path = inside(worktree, &f.path)?;
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {}", f.path))?;
        let rewritten = rewrite_version(f.kind, &text, new)
            .with_context(|| format!("rewrite the version in {}", f.path))?;
        staged.push((path, rewritten));
    }
    if let (Some((path, _)), Some(text)) = (&lock, new_lock) {
        staged.push((path.clone(), text));
    }
    // Everything computed first: a failure above leaves the tree untouched.
    for (path, text) in staged {
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vf(path: &str, kind: VersionKind) -> VersionFile {
        VersionFile {
            path: path.to_owned(),
            kind,
        }
    }

    fn shun() -> Vec<VersionFile> {
        vec![
            vf("src-tauri/Cargo.toml", VersionKind::Cargo),
            vf("package.json", VersionKind::Json),
            vf("src-tauri/tauri.conf.json", VersionKind::Json),
        ]
    }

    #[test]
    fn a_shun_shaped_list_is_valid_and_an_empty_one_is_the_default() {
        validate(&shun()).unwrap();
        validate(&[]).unwrap();
        assert_eq!(primary_path(&[]), "Cargo.toml");
        assert_eq!(primary_path(&shun()), "src-tauri/Cargo.toml");
        assert_eq!(lock_path("src-tauri/Cargo.toml"), "src-tauri/Cargo.lock");
        assert_eq!(lock_path("Cargo.toml"), "Cargo.lock");
    }

    #[test]
    fn malformed_lists_are_refused() {
        let bad = |v: Vec<VersionFile>| validate(&v).is_err();
        assert!(bad(vec![vf("package.json", VersionKind::Json)]));
        assert!(bad(vec![vf("package.json", VersionKind::Cargo)]));
        assert!(bad(vec![vf("/etc/Cargo.toml", VersionKind::Cargo)]));
        assert!(bad(vec![vf("../Cargo.toml", VersionKind::Cargo)]));
        assert!(bad(vec![vf("a/../Cargo.toml", VersionKind::Cargo)]));
        assert!(bad(vec![vf("C:/Cargo.toml", VersionKind::Cargo)]));
        assert!(bad(vec![
            vf("Cargo.toml", VersionKind::Cargo),
            vf("Cargo.toml", VersionKind::Cargo)
        ]));
        assert!(bad(vec![
            vf("Cargo.toml", VersionKind::Cargo),
            vf("x.txt", VersionKind::Json)
        ]));
    }

    #[test]
    fn a_json_version_is_replaced_in_place_and_only_at_the_top_level() {
        let text = "{\n  \"name\": \"shun\",\n  \"dependencies\": { \"version\": \"9\" },\n  \"version\": \"1.2.3\",\n  \"x\": [\"version\"]\n}\n";
        assert_eq!(read_version(VersionKind::Json, text).unwrap(), "1.2.3");
        let out = rewrite_version(VersionKind::Json, text, "1.3.0").unwrap();
        assert_eq!(out, text.replace("1.2.3", "1.3.0"));
        assert!(read_version(VersionKind::Json, "{\"a\": {\"version\": \"1\"}}").is_err());
        assert!(rewrite_version(VersionKind::Json, "{}", "1.0.0").is_err());
    }

    #[test]
    fn a_lock_entry_is_moved_as_text_and_exactly_once() {
        let lock = "version = 4\n\n[[package]]\nname = \"other\"\nversion = \"1.0.0\"\n\n[[package]]\nname = \"shun\"\nversion = \"1.0.0\"\ndependencies = [\"other\"]\n";
        let out = rewrite_lock(lock, "shun", "1.0.0", "1.1.0").unwrap();
        assert_eq!(
            out,
            lock.replace(
                "name = \"shun\"\nversion = \"1.0.0\"",
                "name = \"shun\"\nversion = \"1.1.0\""
            )
        );
        assert!(rewrite_lock(lock, "missing", "1.0.0", "1.1.0").is_err());
        assert!(rewrite_lock(lock, "shun", "9.9.9", "1.1.0").is_err());
    }

    #[test]
    fn disagreeing_versions_are_not_resolved() {
        let same = vec![
            ("a".to_owned(), "1.0.0".to_owned()),
            ("b".to_owned(), "1.0.0".to_owned()),
        ];
        assert_eq!(agree(&same).unwrap(), "1.0.0");
        let diff = vec![
            ("a".to_owned(), "1.0.0".to_owned()),
            ("b".to_owned(), "0.9.0".to_owned()),
        ];
        assert!(agree(&diff).is_err());
    }

    #[test]
    fn release_only_covers_every_declared_file_and_the_lock_beside_the_primary() {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        assert!(release_only(&s(&["Cargo.toml", "Cargo.lock"]), &[]));
        assert!(!release_only(&s(&["package.json"]), &[]));
        assert!(release_only(
            &s(&[
                "package.json",
                "src-tauri/Cargo.lock",
                "src-tauri/tauri.conf.json"
            ]),
            &shun()
        ));
        assert!(!release_only(&s(&["package.json", "src/app.ts"]), &shun()));
        assert!(!release_only(&[], &shun()));
    }

    #[test]
    fn apply_rewrites_every_file_and_the_lock_and_leaves_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, t: &str| {
            let f = d.path().join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, t).unwrap();
        };
        w(
            "src-tauri/Cargo.toml",
            "[package]\nname = \"shun\"\nversion = \"1.0.0\"\n",
        );
        w(
            "src-tauri/Cargo.lock",
            "[[package]]\nname = \"shun\"\nversion = \"1.0.0\"\n",
        );
        w("package.json", "{\n  \"version\": \"1.0.0\"\n}\n");
        w(
            "src-tauri/tauri.conf.json",
            "{ \"version\": \"1.0.0\", \"a\": 1 }",
        );
        apply(d.path(), &shun(), "1.0.1").unwrap();
        let r = |p: &str| std::fs::read_to_string(d.path().join(p)).unwrap();
        assert!(r("src-tauri/Cargo.toml").contains("version = \"1.0.1\""));
        assert!(r("src-tauri/Cargo.lock").contains("version = \"1.0.1\""));
        assert_eq!(r("package.json"), "{\n  \"version\": \"1.0.1\"\n}\n");
        assert_eq!(
            r("src-tauri/tauri.conf.json"),
            "{ \"version\": \"1.0.1\", \"a\": 1 }"
        );
    }

    #[test]
    fn a_failing_file_leaves_the_tree_untouched() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src-tauri")).unwrap();
        let toml = "[package]\nname = \"shun\"\nversion = \"1.0.0\"\n";
        std::fs::write(d.path().join("src-tauri/Cargo.toml"), toml).unwrap();
        std::fs::write(d.path().join("package.json"), "{}").unwrap();
        std::fs::write(
            d.path().join("src-tauri/tauri.conf.json"),
            "{\"version\":\"1.0.0\"}",
        )
        .unwrap();
        assert!(apply(d.path(), &shun(), "1.0.1").is_err());
        assert_eq!(
            std::fs::read_to_string(d.path().join("src-tauri/Cargo.toml")).unwrap(),
            toml
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_leaving_the_repository_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("p.json"), "{\"version\":\"1.0.0\"}").unwrap();
        std::os::unix::fs::symlink(outside.path().join("p.json"), d.path().join("package.json"))
            .unwrap();
        assert!(inside(d.path(), "package.json").is_err());
    }
}
