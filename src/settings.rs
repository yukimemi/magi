//! The machine-config settings screen: which agent each role resolves to, and
//! a safe way to change those assignments.
//!
//! Reading goes through [`Config::load_layers`] like every other consumer, so
//! the screen shows what a run started now would see. Writing touches **one
//! file, the machine layer**, whose path is resolved here from
//! [`Config::machine_layer`] (or injected by a test) and never taken from the
//! client - a repository's `magi.toml` cannot become a write target.
//!
//! `toml_edit` is not a dependency, so the write is a line-level patch of the
//! `[roles]` table: every byte outside the keys being changed (comments,
//! `[vars]`, Tera expressions, other tables) is carried over untouched. The
//! patch is only a proposal. It is written to a temporary file beside the
//! original, the layered config is re-loaded with that file standing in for
//! the machine layer, and the result must say exactly what was asked for
//! before the original is replaced by a rename. Anything the line patcher
//! cannot place (an inline `roles = { .. }`, say) fails that check and is
//! refused with a readable message instead of being guessed at.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use crate::config::{AgentChoice, AgentSpec, Config};

/// The `[roles]` keys the screen edits, in display order.
pub(crate) const ROLE_KEYS: [&str; 5] = [
    "implementers",
    "judges",
    "reviewers",
    "advisors",
    "synthesizer",
];

/// Serializes saves: a read-modify-write of one file is not safe to interleave.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// What `GET /api/settings` returns.
#[derive(Debug, Serialize)]
pub(crate) struct SettingsView {
    /// The repository the effective config was resolved against.
    pub(crate) repo: String,
    pub(crate) machine: MachineView,
    /// Fingerprint of the machine file's bytes; a save must quote it.
    pub(crate) revision: String,
    /// Set when the layered config does not load. `roles` and `agents` are
    /// then empty *because nothing could be read*, and the screen says so.
    pub(crate) error: Option<ConfigError>,
    pub(crate) roles: Vec<RoleView>,
    pub(crate) agents: Vec<AgentView>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MachineView {
    /// Where a save would write, when there is such a place.
    pub(crate) path: Option<String>,
    pub(crate) exists: bool,
    /// Why nothing can be saved, when that is so.
    pub(crate) unavailable: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConfigError {
    pub(crate) message: String,
    /// The layer that fails on its own, when one does.
    pub(crate) path: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RoleView {
    pub(crate) key: &'static str,
    /// The ids the config names, in order. Empty means unset.
    pub(crate) configured: Vec<String>,
    /// `machine`, `repo` or `default`.
    pub(crate) source: &'static str,
    pub(crate) source_path: Option<String>,
    /// Whether a machine-file save can change what a run sees.
    pub(crate) editable: bool,
    pub(crate) locked_reason: Option<String>,
    /// `judges` for advisors that are unset.
    pub(crate) fallback: Option<&'static str>,
    /// The seats a run started now would fill, in order.
    pub(crate) seats: Vec<String>,
    pub(crate) seats_error: Option<String>,
    /// Synthesizer ids that cannot run here (not in the roster, or not installed).
    pub(crate) skipped: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct AgentView {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) model: Option<String>,
    /// `machine`, `repo` or `detected` (found on `PATH`, written by nobody).
    pub(crate) source: &'static str,
    pub(crate) source_path: Option<String>,
}

/// Why a save did not happen.
#[derive(Debug)]
pub(crate) enum SaveError {
    /// The machine file changed since the client read it.
    Conflict(String),
    /// The request or the resulting config is not acceptable.
    Refused(String),
    /// The disk said no.
    Internal(String),
}

/// FNV-1a of the file's bytes, hex. Same function family as `notices::id_of`.
fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn repo_layers(repo: &Path) -> Vec<PathBuf> {
    [
        repo.join(".magi").join("config.toml"),
        repo.join("magi.toml"),
    ]
    .into_iter()
    .filter(|p| p.is_file())
    .collect()
}

/// Every layer that applies, machine first - [`Config::layers`] with the
/// machine path injected.
fn layer_paths(repo: &Path, machine: Option<&Path>) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = machine
        .filter(|m| m.is_file())
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    paths.extend(repo_layers(repo));
    paths
}

fn effective(paths: &[PathBuf]) -> anyhow::Result<Config> {
    if paths.is_empty() {
        return Ok(Config::autodetected());
    }
    Config::load_layers(paths)
}

fn declares(table: &toml::Table, key: &str) -> bool {
    table
        .get("roles")
        .and_then(toml::Value::as_table)
        .is_some_and(|r| r.contains_key(key))
}

fn defines_agents(table: &toml::Table) -> bool {
    table
        .get("agents")
        .and_then(toml::Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

fn choice_ids(choice: Option<&AgentChoice>) -> Vec<String> {
    choice
        .map(|c| c.ids().into_iter().map(str::to_owned).collect())
        .unwrap_or_default()
}

fn role_ids(cfg: &Config, key: &str) -> Vec<String> {
    match key {
        "implementers" => cfg.roles.implementers.clone(),
        "judges" => cfg.roles.judges.clone(),
        "reviewers" => cfg.roles.reviewers.clone(),
        "advisors" => cfg.roles.advisors.clone(),
        _ => choice_ids(cfg.roles.synthesizer.as_ref()),
    }
}

fn ids_of(specs: &[AgentSpec]) -> Vec<String> {
    specs.iter().map(|s| s.id.clone()).collect()
}

/// The effective roles and roster for `repo`, with the machine layer at
/// `machine`.
pub(crate) fn view(repo: &Path, machine: Option<&Path>) -> SettingsView {
    let bytes = machine
        .and_then(|m| std::fs::read(m).ok())
        .unwrap_or_default();
    let mut out = SettingsView {
        repo: repo.display().to_string(),
        machine: MachineView {
            path: machine.map(|m| m.display().to_string()),
            exists: machine.is_some_and(Path::is_file),
            unavailable: machine.is_none().then(|| {
                "This machine has no machine-config location (the config directory is \
                 unset or empty), so nothing can be saved from here."
                    .to_owned()
            }),
        },
        revision: fingerprint(&bytes),
        error: None,
        roles: Vec::new(),
        agents: Vec::new(),
    };
    let paths = layer_paths(repo, machine);
    let cfg = match effective(&paths) {
        Ok(cfg) => cfg,
        Err(e) => {
            let failing = Config::layer_tables(&paths)
                .into_iter()
                .find(|(_, t)| t.is_err())
                .map(|(p, _)| p.display().to_string());
            out.error = Some(ConfigError {
                message: format!("{e:#}"),
                path: failing,
            });
            return out;
        }
    };
    let tables: Vec<(PathBuf, toml::Table)> = Config::layer_tables(&paths)
        .into_iter()
        .filter_map(|(p, t)| t.ok().map(|t| (p, t)))
        .collect();
    let is_machine = |p: &Path| machine.is_some_and(|m| m == p);

    let resolved = cfg.resolve_roles();
    let advisors = cfg.advisors();
    for key in ROLE_KEYS {
        let configured = role_ids(&cfg, key);
        let owner = tables.iter().rev().find(|(_, t)| declares(t, key));
        let (source, source_path) = match owner {
            Some((p, _)) if is_machine(p) => ("machine", Some(p.display().to_string())),
            Some((p, _)) => ("repo", Some(p.display().to_string())),
            None => ("default", None),
        };
        let repo_owner = tables
            .iter()
            .find(|(p, t)| !is_machine(p) && declares(t, key));
        let (editable, locked_reason) = if let Some((p, _)) = repo_owner {
            (
                false,
                Some(format!(
                    "This repository decides it: `{key}` is set in {}. Change it there; \
                     a machine setting would be overridden or make the config invalid.",
                    p.display()
                )),
            )
        } else if machine.is_none() {
            (false, out.machine.unavailable.clone())
        } else {
            (true, None)
        };
        let mut view = RoleView {
            key,
            configured,
            source,
            source_path,
            editable,
            locked_reason,
            fallback: None,
            seats: Vec::new(),
            seats_error: None,
            skipped: Vec::new(),
        };
        let seats = match key {
            "implementers" => resolved
                .as_ref()
                .map(|r| ids_of(&r.implementers))
                .map_err(|e| anyhow::anyhow!("{e:#}")),
            "judges" => resolved
                .as_ref()
                .map(|r| ids_of(&r.judges))
                .map_err(|e| anyhow::anyhow!("{e:#}")),
            "reviewers" => resolved
                .as_ref()
                .map(|r| ids_of(&r.reviewers))
                .map_err(|e| anyhow::anyhow!("{e:#}")),
            "advisors" => {
                if view.configured.is_empty() {
                    view.fallback = Some("judges");
                }
                advisors
                    .as_ref()
                    .map(|a| ids_of(a))
                    .map_err(|e| anyhow::anyhow!("{e:#}"))
            }
            _ => crate::agent::pick_chain(
                &cfg.agents,
                cfg.roles.synthesizer.as_ref(),
                &crate::agent::installed,
                "synthesizer",
            )
            .map(|chain| {
                let ids = ids_of(&chain);
                view.skipped = view
                    .configured
                    .iter()
                    .filter(|id| !ids.contains(id))
                    .cloned()
                    .collect();
                ids
            }),
        };
        match seats {
            Ok(ids) => view.seats = ids,
            Err(e) => view.seats_error = Some(format!("{e:#}")),
        }
        out.roles.push(view);
    }

    out.agents = cfg
        .agents
        .iter()
        .map(|a| {
            let owner = tables.iter().rev().find(|(_, t)| {
                t.get("agents")
                    .and_then(toml::Value::as_array)
                    .is_some_and(|list| {
                        list.iter()
                            .any(|x| x.get("id").and_then(toml::Value::as_str) == Some(&a.id))
                    })
            });
            let (source, source_path) = match owner {
                Some((p, _)) if is_machine(p) => ("machine", Some(p.display().to_string())),
                Some((p, _)) => ("repo", Some(p.display().to_string())),
                None => ("detected", None),
            };
            AgentView {
                id: a.id.clone(),
                kind: toml::Value::try_from(a.kind)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                model: a.model.clone(),
                source,
                source_path,
            }
        })
        .collect();
    out
}

/// Replace the given `[roles]` keys in the machine file at `machine`.
///
/// `roles` maps a key to its new ids; an empty list removes the key (back to
/// the default). Keys not named are left exactly as they are.
pub(crate) fn save(
    repo: &Path,
    machine: Option<&Path>,
    revision: &str,
    roles: &BTreeMap<String, Vec<String>>,
) -> Result<SettingsView, SaveError> {
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Some(machine) = machine else {
        return Err(SaveError::Refused(
            "This machine has no machine-config location, so there is nowhere to save to."
                .to_owned(),
        ));
    };
    let current = view(repo, Some(machine));
    if let Some(err) = &current.error {
        return Err(SaveError::Refused(format!(
            "The current config does not load, so it cannot be edited safely: {}",
            err.message
        )));
    }
    if current.revision != revision {
        return Err(SaveError::Conflict(
            "The machine config changed since this screen loaded it. Reload and apply \
             the change again."
                .to_owned(),
        ));
    }
    if roles.is_empty() {
        return Err(SaveError::Refused("Nothing to change.".to_owned()));
    }
    let known: Vec<&str> = current.agents.iter().map(|a| a.id.as_str()).collect();
    let mut edits: Vec<(&'static str, Vec<String>)> = Vec::new();
    for (key, ids) in roles {
        let Some(role) = current.roles.iter().find(|r| r.key == key) else {
            return Err(SaveError::Refused(format!(
                "`{key}` is not a role this screen edits."
            )));
        };
        if !role.editable {
            return Err(SaveError::Refused(
                role.locked_reason
                    .clone()
                    .unwrap_or_else(|| format!("`{key}` cannot be edited here.")),
            ));
        }
        let ids: Vec<String> = ids.iter().map(|i| i.trim().to_owned()).collect();
        if let Some(bad) = ids
            .iter()
            .find(|i| i.is_empty() || !known.contains(&i.as_str()))
        {
            return Err(SaveError::Refused(format!(
                "`{bad}` is not a defined agent. Defined: {}.",
                known.join(", ")
            )));
        }
        edits.push((role.key, ids));
    }

    let original = match std::fs::read_to_string(machine) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(SaveError::Internal(format!(
                "reading {}: {e}",
                machine.display()
            )));
        }
    };
    let mut text = original.clone();
    for (key, ids) in &edits {
        text = patch_role(&text, key, ids).map_err(SaveError::Refused)?;
    }
    // A machine file made here is the only layer that defines agents once it
    // exists, so the roster on screen (found on PATH) is written down with it.
    let agents_defined = Config::layer_tables(&layer_paths(repo, Some(machine)))
        .iter()
        .any(|(_, t)| t.as_ref().is_ok_and(defines_agents));
    if !agents_defined {
        let detected = Config::autodetected();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        for a in &detected.agents {
            text.push_str(&format!("\n[[agents]]\nid = {}\n", quote(&a.id)));
            if let Ok(kind) = toml::Value::try_from(a.kind) {
                text.push_str(&format!("kind = {kind}\n"));
            }
            if let Some(model) = &a.model {
                text.push_str(&format!("model = {}\n", quote(model)));
            }
        }
    }

    let dir = machine
        .parent()
        .ok_or_else(|| SaveError::Internal("the machine config has no parent directory".into()))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| SaveError::Internal(format!("creating {}: {e}", dir.display())))?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(".config.toml.{}.{nonce}.tmp", std::process::id()));
    let cleanup = |e: SaveError| {
        let _ = std::fs::remove_file(&tmp);
        e
    };
    write_synced(&tmp, &text).map_err(|e| {
        cleanup(SaveError::Internal(format!(
            "writing {}: {e}",
            tmp.display()
        )))
    })?;

    // Validate the real thing: the layered load with the proposal standing in
    // for the machine file.
    let mut layers = vec![tmp.clone()];
    layers.extend(repo_layers(repo));
    let loaded = Config::load_layers(&layers).map_err(|e| {
        cleanup(SaveError::Refused(format!(
            "The change would leave the config unloadable, so nothing was saved: {e:#}"
        )))
    })?;
    for (key, ids) in &edits {
        let got = role_ids(&loaded, key);
        if &got != ids {
            return Err(cleanup(SaveError::Refused(format!(
                "The change would not take effect as asked: `{key}` would resolve to [{}] \
                 instead of [{}] (an include or a template in the machine file overrides \
                 it). Nothing was saved.",
                got.join(", "),
                ids.join(", ")
            ))));
        }
    }
    for key in ROLE_KEYS {
        if let Some(bad) = role_ids(&loaded, key)
            .into_iter()
            .find(|id| loaded.agent(id).is_err())
        {
            return Err(cleanup(SaveError::Refused(format!(
                "`{key}` would name `{bad}`, which is not defined. Nothing was saved."
            ))));
        }
    }
    std::fs::rename(&tmp, machine).map_err(|e| {
        cleanup(SaveError::Internal(format!(
            "replacing {}: {e}",
            machine.display()
        )))
    })?;
    Ok(view(repo, Some(machine)))
}

fn write_synced(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()
}

fn quote(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

/// The value text for `key`: a bare string for a one-agent synthesizer, an
/// array otherwise.
fn value_text(key: &str, ids: &[String]) -> String {
    if key == "synthesizer" && ids.len() == 1 {
        return quote(&ids[0]);
    }
    let items: Vec<String> = ids.iter().map(|i| quote(i)).collect();
    format!("[{}]", items.join(", "))
}

/// Walk one line's value text, tracking bracket depth outside strings.
/// Returns the byte offset of a trailing comment, if any.
fn scan_line(line: &str, depth: &mut i32) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match quote {
            Some('"') if escaped => escaped = false,
            Some('"') if c == '\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '{' => *depth += 1,
                ']' | '}' => *depth -= 1,
                '#' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

/// The key a `key = value` line assigns, bare or quoted.
fn assigned_key(trimmed: &str) -> Option<(String, usize)> {
    let eq = trimmed.find('=')?;
    let raw = trimmed[..eq].trim();
    let key = raw.trim_matches(|c| c == '"' || c == '\'');
    let simple = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    simple.then(|| (key.to_owned(), eq + 1))
}

fn is_roles_header(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix('[') else {
        return false;
    };
    if rest.starts_with('[') {
        return false;
    }
    rest.split(']')
        .next()
        .is_some_and(|n| n.trim().trim_matches(|c| c == '"' || c == '\'') == "roles")
}

/// Patch one key of the `[roles]` table, leaving every other byte alone.
fn patch_role(text: &str, key: &str, ids: &[String]) -> Result<String, String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    // Section bounds and the key's span, found in one pass that knows about
    // multi-line arrays.
    let mut depth = 0;
    let mut in_roles = false;
    let mut header: Option<usize> = None;
    let mut last_key_end: Option<usize> = None;
    let mut span: Option<(usize, usize, Option<String>)> = None;
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if depth == 0 && trimmed.starts_with('[') {
            in_roles = header.is_none() && is_roles_header(trimmed);
            if in_roles {
                header = Some(i);
            } else if header.is_some() {
                break;
            }
            i += 1;
            continue;
        }
        if depth == 0
            && let Some((name, _)) = assigned_key(trimmed)
        {
            let start = i;
            let mut comment = None;
            let first = lines[i].trim_start();
            let at = first.find('=').map_or(0, |p| p + 1);
            let mut end = i;
            let mut hash = scan_line(&first[at..], &mut depth);
            if in_roles
                && name == key
                && (first[at..].contains("\"\"\"") || first[at..].contains("'''"))
            {
                return Err(format!(
                    "`{key}` uses a multi-line string; edit it by hand."
                ));
            }
            while depth > 0 && end + 1 < lines.len() {
                end += 1;
                hash = scan_line(lines[end], &mut depth);
            }
            if let Some(h) = hash {
                let line = if end == start {
                    &first[at..]
                } else {
                    lines[end]
                };
                comment = Some(line[h..].trim_end().to_owned());
            }
            if in_roles {
                last_key_end = Some(end);
                if name == key {
                    let body = lines[start..=end].concat();
                    if body.contains("{{") || body.contains("{%") {
                        return Err(format!(
                            "`{key}` is written with a template expression, which this screen \
                             cannot edit without losing it. Change it by hand."
                        ));
                    }
                    span = Some((start, end, comment));
                }
            }
            i = end + 1;
            continue;
        }
        if depth > 0 {
            scan_line(lines[i], &mut depth);
        }
        i += 1;
    }

    let mut out: Vec<String> = lines.iter().map(|l| (*l).to_owned()).collect();
    let new_line = |indent: &str, comment: Option<&str>| {
        let mut s = format!("{indent}{key} = {}", value_text(key, ids));
        if let Some(c) = comment {
            s.push_str("  ");
            s.push_str(c);
        }
        s.push_str(eol);
        s
    };
    match (span, ids.is_empty()) {
        (Some((start, end, _)), true) => {
            out.drain(start..=end);
        }
        (Some((start, end, comment)), false) => {
            let indent: String = lines[start]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            out.splice(start..=end, [new_line(&indent, comment.as_deref())]);
        }
        (None, true) => {}
        (None, false) => match (header, last_key_end) {
            (Some(_), Some(end)) | (Some(end), None) => {
                if !out[end].ends_with('\n') {
                    out[end].push_str(eol);
                }
                out.insert(end + 1, new_line("", None));
            }
            (None, _) => {
                if let Some(last) = out.last_mut()
                    && !last.ends_with('\n')
                {
                    last.push_str(eol);
                }
                if !out.is_empty() {
                    out.push(eol.to_owned());
                }
                out.push(format!("[roles]{eol}"));
                out.push(new_line("", None));
            }
        },
    }
    Ok(out.concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    const AGENTS: &str = "[[agents]]\nid = \"a\"\nkind = \"command\"\ncommand = [\"true\"]\n\n\
                          [[agents]]\nid = \"b\"\nkind = \"command\"\ncommand = [\"true\"]\n";

    #[test]
    fn patch_keeps_comments_and_unrelated_keys() {
        let src = "# top comment\n[vars]\ncache = \"/x\"  # keep\n\n[roles]\n# why\nimplementers = [\n  \"a\", # first\n  \"b\",\n]  # tail\njudges = [\"a\"]\nfixer = \"a\"\n\n[graph]\ncandidates = 2\n";
        let out = patch_role(src, "implementers", &ids(&["b", "a"])).unwrap();
        assert_eq!(
            out,
            "# top comment\n[vars]\ncache = \"/x\"  # keep\n\n[roles]\n# why\nimplementers = [\"b\", \"a\"]  # tail\njudges = [\"a\"]\nfixer = \"a\"\n\n[graph]\ncandidates = 2\n"
        );
        let out = patch_role(&out, "judges", &[]).unwrap();
        assert!(!out.contains("judges"));
        assert!(out.contains("fixer = \"a\"") && out.contains("[graph]"));
    }

    #[test]
    fn patch_creates_the_table_and_inserts_into_it() {
        let out = patch_role("[vars]\nx = 1", "judges", &ids(&["a"])).unwrap();
        assert_eq!(out, "[vars]\nx = 1\n\n[roles]\njudges = [\"a\"]\n");
        let out = patch_role(
            "[roles]\nfixer = \"a\"\n\n[graph]\njudges = 3\n",
            "judges",
            &ids(&["a"]),
        )
        .unwrap();
        assert_eq!(
            out,
            "[roles]\nfixer = \"a\"\njudges = [\"a\"]\n\n[graph]\njudges = 3\n"
        );
        // `[graph] judges` is not `[roles] judges`.
        let out = patch_role("[graph]\njudges = 3\n", "judges", &[]).unwrap();
        assert_eq!(out, "[graph]\njudges = 3\n");
    }

    #[test]
    fn synthesizer_is_a_string_for_one_and_an_array_for_a_chain() {
        let one = patch_role("", "synthesizer", &ids(&["a"])).unwrap();
        assert!(one.contains("synthesizer = \"a\""), "{one}");
        let two = patch_role("", "synthesizer", &ids(&["a", "b"])).unwrap();
        assert!(two.contains("synthesizer = [\"a\", \"b\"]"), "{two}");
    }

    #[test]
    fn patch_refuses_a_templated_value() {
        let src = "[roles]\njudges = [\"{{ env.X | default(value='a') }}\"]\n";
        assert!(patch_role(src, "judges", &ids(&["a"])).is_err());
    }

    #[test]
    fn save_writes_machine_file_only_and_keeps_comments() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let repo_toml = format!("{AGENTS}\n[graph]\ncandidates = 1\n");
        std::fs::write(repo.join("magi.toml"), &repo_toml).unwrap();
        let machine = tmp.path().join("cfg").join("magi").join("config.toml");
        std::fs::create_dir_all(machine.parent().unwrap()).unwrap();
        std::fs::write(
            &machine,
            "# mine\n[roles]\n# seats\njudges = [\"a\"] # note\n\n[vars]\nx = 1\n",
        )
        .unwrap();

        let v = view(&repo, Some(&machine));
        assert!(v.error.is_none(), "{:?}", v.error);
        let r = save(
            &repo,
            Some(&machine),
            &v.revision,
            &BTreeMap::from([("judges".to_owned(), ids(&["b", "a"]))]),
        )
        .unwrap();
        let text = std::fs::read_to_string(&machine).unwrap();
        assert_eq!(
            text,
            "# mine\n[roles]\n# seats\njudges = [\"b\", \"a\"]  # note\n\n[vars]\nx = 1\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("magi.toml")).unwrap(),
            repo_toml
        );
        let judges = r.roles.iter().find(|x| x.key == "judges").unwrap();
        assert_eq!(judges.source, "machine");
        assert_eq!(judges.configured, ids(&["b", "a"]));
        // No tmp file left behind.
        let leftovers = std::fs::read_dir(machine.parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(leftovers, 1);
    }

    #[test]
    fn save_refuses_unknown_ids_and_leaves_the_file_alone() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("magi.toml"), AGENTS).unwrap();
        let machine = tmp.path().join("m").join("magi").join("config.toml");
        let v = view(&repo, Some(&machine));
        let err = save(
            &repo,
            Some(&machine),
            &v.revision,
            &BTreeMap::from([("judges".to_owned(), ids(&["nope"]))]),
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::Refused(m) if m.contains("nope")));
        assert!(!machine.exists());
    }

    #[test]
    fn save_refuses_a_key_the_repo_owns_and_a_stale_revision() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("magi.toml"),
            format!("{AGENTS}\n[roles]\njudges = [\"a\"]\n"),
        )
        .unwrap();
        let machine = tmp.path().join("m").join("magi").join("config.toml");
        let v = view(&repo, Some(&machine));
        let j = v.roles.iter().find(|r| r.key == "judges").unwrap();
        assert!(!j.editable && j.source == "repo");
        let want = BTreeMap::from([("judges".to_owned(), ids(&["b"]))]);
        assert!(matches!(
            save(&repo, Some(&machine), &v.revision, &want),
            Err(SaveError::Refused(_))
        ));
        let want = BTreeMap::from([("reviewers".to_owned(), ids(&["b"]))]);
        assert!(matches!(
            save(&repo, Some(&machine), "stale", &want),
            Err(SaveError::Conflict(_))
        ));
    }

    #[test]
    fn a_config_that_does_not_parse_is_an_error_not_an_empty_list() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("magi.toml"), "[roles\nbroken").unwrap();
        let v = view(&repo, None);
        let e = v.error.expect("an error");
        assert!(
            e.path.is_some_and(|p| p.ends_with("magi.toml")),
            "{}",
            e.message
        );
        assert!(v.roles.is_empty() && v.agents.is_empty());
    }

    #[test]
    fn advisors_unset_falls_back_to_judges() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join("magi.toml"),
            format!("{AGENTS}\n[roles]\njudges = [\"b\"]\n"),
        )
        .unwrap();
        let v = view(&repo, None);
        let a = v.roles.iter().find(|r| r.key == "advisors").unwrap();
        assert_eq!(a.fallback, Some("judges"));
        assert_eq!(a.source, "default");
        assert!(
            a.seats.iter().all(|s| s == "b") && !a.seats.is_empty(),
            "{:?}",
            a.seats
        );
    }
}
