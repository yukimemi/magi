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
//!
//! The machine file applies to every repository, so two more rules hold. The
//! detected roster is never written down (`Config::load_layers` fills `agents`
//! from `PATH` whenever no layer declares the key), so a role save adds no
//! second `[[agents]]` declaration and CLIs installed later are still found.
//! And a save is also loaded against every other checkout found under
//! `[repos] roots`: one that loaded before and would not now (it declares the
//! same `roles.*` key) refuses the save in words. Limits: checkouts outside
//! `roots` are not checked, a checkout already broken is ignored, and another
//! process editing a repo config between the check and the rename is not
//! prevented. The view shows the same lock up front.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;

use crate::config::{AgentChoice, AgentSpec, Config};

/// The `[roles]` keys the screen edits, in display order.
pub(crate) const ROLE_KEYS: [&str; 6] = [
    "implementers",
    "judges",
    "reviewers",
    "advisors",
    "synthesizer",
    "fixer",
];

/// The roles that take one id as a bare string and several as a chain.
fn is_chain_role(key: &str) -> bool {
    matches!(key, "synthesizer" | "fixer")
}

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
    Config::repo_layers(repo).unwrap_or_else(|e| {
        tracing::warn!("could not read the repository's config layers: {e:#}");
        Vec::new()
    })
}

/// Every layer that applies to `repo` itself, machine first; an unreadable
/// remote ref is an error, never an empty list (settings for an unknown config
/// must not be editable).
fn layer_paths_strict(repo: &Path, machine: Option<&Path>) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = machine
        .filter(|m| m.is_file())
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    paths.extend(Config::repo_layers(repo)?);
    Ok(paths)
}

/// Every layer that applies, machine first - [`Config::layers`] with the
/// machine path injected. Lenient: for *other* checkouts, whose own failures
/// are not this screen's business.
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
        "fixer" => choice_ids(cfg.roles.fixer.as_ref()),
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
    let paths = match layer_paths_strict(repo, machine) {
        Ok(p) => p,
        Err(e) => {
            out.error = Some(ConfigError {
                message: format!("{e:#}"),
                path: None,
            });
            return out;
        }
    };
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

    let mut other_declares: BTreeMap<&str, String> = BTreeMap::new();
    for other in other_checkouts(repo, &cfg.repos.roots) {
        let theirs = Config::layer_tables(&repo_layers(&other.path));
        for key in ROLE_KEYS {
            if theirs
                .iter()
                .any(|(_, t)| t.as_ref().is_ok_and(|t| declares(t, key)))
            {
                other_declares
                    .entry(key)
                    .or_insert_with(|| other.name.clone());
            }
        }
    }
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
                    "This repo overrides roles.{key} in {} - edit it there.",
                    p.display()
                )),
            )
        } else if let Some(name) = other_declares.get(key) {
            (
                false,
                Some(format!(
                    "`{name}` declares roles.{key} in its own config, so a machine setting \
                     would stop it from loading. Edit it there."
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
            // Unset keeps the winner's own author: no chain to show.
            "fixer" if cfg.roles.fixer.is_none() => {
                view.fallback = Some("winner's implementer");
                Ok(Vec::new())
            }
            "fixer" => crate::agent::pick_chain(
                &cfg.agents,
                cfg.roles.fixer.as_ref(),
                &crate::agent::installed,
                "fixer",
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
    match Config::repo_layers(repo) {
        Ok(l) => layers.extend(l),
        Err(e) => {
            return Err(cleanup(SaveError::Refused(format!(
                "The repository's config cannot be read, so nothing was saved: {e:#}"
            ))));
        }
    }
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
    other_repos_still_load(repo, machine, &tmp, &loaded.repos.roots).map_err(cleanup)?;
    std::fs::rename(&tmp, machine).map_err(|e| {
        cleanup(SaveError::Internal(format!(
            "replacing {}: {e}",
            machine.display()
        )))
    })?;
    Ok(view(repo, Some(machine)))
}

/// Every checkout under `roots` other than `repo` that has its own config
/// layers.
fn other_checkouts(repo: &Path, roots: &[PathBuf]) -> Vec<crate::repos::Repo> {
    let here = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    crate::repos::scan(roots)
        .into_iter()
        .filter(|r| r.path != here && !repo_layers(&r.path).is_empty())
        .collect()
}

/// The machine file applies to every repository, so a proposal is checked
/// against each other known checkout too: one that loaded with the old machine
/// file and no longer loads with `proposal` (a `roles.*` array now declared in
/// two layers) refuses the save. A checkout that did not load before is not
/// this change's doing and is ignored.
fn other_repos_still_load(
    repo: &Path,
    machine: &Path,
    proposal: &Path,
    roots: &[PathBuf],
) -> Result<(), SaveError> {
    for other in other_checkouts(repo, roots) {
        let layers = repo_layers(&other.path);
        let mut old = layer_paths(&other.path, Some(machine));
        if old.is_empty() {
            old = layers.clone();
        }
        if Config::load_layers(&old).is_err() {
            continue;
        }
        let mut new = vec![proposal.to_path_buf()];
        new.extend(layers);
        if let Err(e) = Config::load_layers(&new) {
            return Err(SaveError::Refused(format!(
                "Nothing was saved: this machine setting would stop `{}` from loading, \
                 because that repository declares the same setting in its own config \
                 ({e:#}). Edit it there, or remove it from that repository first.",
                other.name
            )));
        }
    }
    Ok(())
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

/// The value text for `key`: a bare string for a one-agent chain role
/// (synthesizer, fixer), an array otherwise.
fn value_text(key: &str, ids: &[String]) -> String {
    if is_chain_role(key) && ids.len() == 1 {
        return quote(&ids[0]);
    }
    let items: Vec<String> = ids.iter().map(|i| quote(i)).collect();
    format!("[{}]", items.join(", "))
}

/// Lexer state carried from one line to the next: bracket depth, and the
/// delimiter of a multi-line string still open at the end of the last line.
#[derive(Default)]
struct Scan {
    depth: i32,
    multi: Option<&'static str>,
}

impl Scan {
    fn open(&self) -> bool {
        self.depth > 0 || self.multi.is_some()
    }
}

/// Walk one line, tracking bracket depth outside strings and any multi-line
/// string that opens or closes on it. Returns the byte offset of a trailing
/// comment, if any.
fn scan_line(line: &str, st: &mut Scan) -> Option<usize> {
    let b = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut i = 0;
    while i < b.len() {
        if let Some(delim) = st.multi {
            if b[i..].starts_with(delim.as_bytes()) {
                st.multi = None;
                i += 3;
            } else if delim == "\"\"\"" && b[i] == b'\\' {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        let c = b[i];
        match quote {
            Some(b'"') if escaped => escaped = false,
            Some(b'"') if c == b'\\' => escaped = true,
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => {
                if b[i..].starts_with(b"\"\"\"") {
                    st.multi = Some("\"\"\"");
                    i += 3;
                    continue;
                }
                if b[i..].starts_with(b"'''") {
                    st.multi = Some("'''");
                    i += 3;
                    continue;
                }
                match c {
                    b'"' | b'\'' => quote = Some(c),
                    b'[' | b'{' => st.depth += 1,
                    b']' | b'}' => st.depth -= 1,
                    b'#' => return Some(i),
                    _ => {}
                }
            }
        }
        i += 1;
    }
    None
}

/// The strings quoted on one line, outside its comment.
fn quoted_ids(code: &str) -> Vec<String> {
    code.split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
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

/// What the old value's lines said besides the ids: per line, the ids on it
/// and its comment, so a rewrite can carry the comments over.
struct OldSpan {
    start: usize,
    end: usize,
    /// `(line, ids on the line, comment)` for each line that has a comment.
    notes: Vec<(usize, Vec<String>, String)>,
    /// Every id in order of appearance (duplicates included), with the
    /// comment on its line when it is the last id there - so a comment follows
    /// one occurrence of an id, not every seat of the same agent.
    seats: Vec<(String, Option<String>)>,
}

/// Patch one key of the `[roles]` table, leaving every other byte alone.
///
/// Comments inside the replaced value are kept: a comment on an id's line
/// follows that id, standalone ones stay inside the array, and a trailing
/// comment stays trailing. Only the comment of an id that is removed goes with
/// it - and a reset keeps every comment of the key as plain comment lines.
fn patch_role(text: &str, key: &str, ids: &[String]) -> Result<String, String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.split_inclusive('\n').collect();

    // Section bounds and the key's span, found in one pass that knows about
    // multi-line arrays and multi-line strings.
    let mut st = Scan::default();
    let mut in_roles = false;
    let mut header: Option<usize> = None;
    let mut last_key_end: Option<usize> = None;
    let mut span: Option<OldSpan> = None;
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if !st.open() && trimmed.starts_with('[') {
            in_roles = header.is_none() && is_roles_header(trimmed);
            if in_roles {
                header = Some(i);
            } else if header.is_some() {
                break;
            }
            i += 1;
            continue;
        }
        if !st.open()
            && let Some((name, _)) = assigned_key(trimmed)
        {
            let start = i;
            let first = lines[i].trim_start();
            let at = first.find('=').map_or(0, |p| p + 1);
            let mut end = i;
            let mut notes = Vec::new();
            let mut seats: Vec<(String, Option<String>)> = Vec::new();
            let mut note = |line: usize, code: &str, hash: Option<usize>| {
                let ids = quoted_ids(&code[..hash.unwrap_or(code.len())]);
                let comment = hash.map(|h| code[h..].trim_end().to_owned());
                let last = ids.len().saturating_sub(1);
                for (n, id) in ids.iter().enumerate() {
                    seats.push((id.clone(), comment.clone().filter(|_| n == last)));
                }
                if let Some(c) = comment {
                    notes.push((line, ids, c));
                }
            };
            let code = &first[at..];
            let hash = scan_line(code, &mut st);
            note(start, code, hash);
            while st.open() && end + 1 < lines.len() {
                end += 1;
                let hash = scan_line(lines[end], &mut st);
                note(end, lines[end], hash);
            }
            if in_roles {
                last_key_end = Some(end);
                if name == key {
                    let body = lines[start..=end].concat();
                    if body.contains("\"\"\"") || body.contains("'''") {
                        return Err(format!(
                            "`{key}` uses a multi-line string; edit it by hand."
                        ));
                    }
                    if body.contains("{{") || body.contains("{%") {
                        return Err(format!(
                            "`{key}` is written with a template expression, which this screen \
                             cannot edit without losing it. Change it by hand."
                        ));
                    }
                    span = Some(OldSpan {
                        start,
                        end,
                        notes,
                        seats,
                    });
                }
            }
            i = end + 1;
            continue;
        }
        if st.open() {
            scan_line(lines[i], &mut st);
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
        (Some(old), true) => {
            let indent: String = lines[old.start]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            let kept: Vec<String> = old
                .notes
                .iter()
                .map(|(_, _, c)| format!("{indent}{c}{eol}"))
                .collect();
            out.splice(old.start..=old.end, kept);
        }
        (Some(old), false) => {
            let indent: String = lines[old.start]
                .chars()
                .take_while(|c| c.is_whitespace())
                .collect();
            let single = old.start == old.end;
            // A trailing comment is the last line's: always for a one-line
            // value, otherwise only when that line holds no id (`]  # tail`).
            let trailing = match old.notes.last() {
                Some((line, on_line, c)) if *line == old.end && (single || on_line.is_empty()) => {
                    Some(c.clone())
                }
                _ => None,
            };
            let interior = &old.notes[..old.notes.len() - usize::from(trailing.is_some())];
            let replacement: Vec<String> =
                if interior.is_empty() || (is_chain_role(key) && ids.len() == 1) {
                    // One line. Comments that cannot ride on it stay above it.
                    let mut v: Vec<String> = interior
                        .iter()
                        .map(|(_, _, c)| format!("{indent}{c}{eol}"))
                        .collect();
                    v.push(new_line(&indent, trailing.as_deref()));
                    v
                } else {
                    let mut v = vec![format!("{indent}{key} = [{eol}")];
                    for (_, _, c) in interior.iter().filter(|(_, l, _)| l.is_empty()) {
                        v.push(format!("{indent}  {c}{eol}"));
                    }
                    let mut taken: std::collections::HashMap<&str, usize> = Default::default();
                    for id in ids {
                        let nth = taken.entry(id.as_str()).or_insert(0);
                        let note = old
                            .seats
                            .iter()
                            .filter(|(old_id, _)| old_id == id)
                            .nth(*nth)
                            .and_then(|(_, c)| c.as_ref())
                            .map(|c| format!("  {c}"))
                            .unwrap_or_default();
                        *nth += 1;
                        v.push(format!("{indent}  {},{note}{eol}", quote(id)));
                    }
                    v.push(format!(
                        "{indent}]{}{eol}",
                        trailing.map(|c| format!("  {c}")).unwrap_or_default()
                    ));
                    v
                };
            out.splice(old.start..=old.end, replacement);
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
            "# top comment\n[vars]\ncache = \"/x\"  # keep\n\n[roles]\n# why\nimplementers = [\n  \"b\",\n  \"a\",  # first\n]  # tail\njudges = [\"a\"]\nfixer = \"a\"\n\n[graph]\ncandidates = 2\n"
        );
        let out = patch_role(&out, "judges", &[]).unwrap();
        assert!(!out.contains("judges"));
        assert!(out.contains("fixer = \"a\"") && out.contains("[graph]"));
    }

    #[test]
    fn patch_does_not_read_a_role_table_out_of_a_multiline_string() {
        let src = "[vars]\nexample = \'\'\'\n[roles]\njudges = [\"a\"]\n\'\'\'\nother = \"\"\"\n[roles]\n\"\"\"\n";
        // No real [roles] table: removing is a no-op, adding creates one.
        assert_eq!(patch_role(src, "judges", &[]).unwrap(), src);
        let out = patch_role(src, "judges", &ids(&["a"])).unwrap();
        assert!(out.starts_with(src), "{out}");
        assert!(out.ends_with("\n[roles]\njudges = [\"a\"]\n"), "{out}");
    }

    #[test]
    fn duplicate_seats_keep_their_own_comments() {
        let src =
            "[roles]\njudges = [\n  \"a\", # first seat\n  \"a\", # second seat\n  \"b\",\n]\n";
        let out = patch_role(src, "judges", &ids(&["b", "a", "a"])).unwrap();
        assert_eq!(
            out,
            "[roles]\njudges = [\n  \"b\",\n  \"a\",  # first seat\n  \"a\",  # second seat\n]\n"
        );
    }

    #[test]
    fn a_reset_keeps_the_comments_of_the_removed_key() {
        let out = patch_role(
            "[roles]\njudges = [\"a\"]  # why a\nfixer = \"a\"\n",
            "judges",
            &[],
        )
        .unwrap();
        assert_eq!(out, "[roles]\n# why a\nfixer = \"a\"\n");
        let out = patch_role(
            "[roles]\njudges = [\n  # lead\n  \"a\", # why a\n]\n",
            "judges",
            &[],
        )
        .unwrap();
        assert_eq!(out, "[roles]\n# lead\n# why a\n");
    }

    #[test]
    fn a_comment_follows_its_id_through_a_reorder() {
        let src =
            "[roles]\njudges = [ # head\n  # lead\n  \"a\", # why a\n  \"b\", # why b\n]  # tail\n";
        let out = patch_role(src, "judges", &ids(&["b", "c", "a"])).unwrap();
        assert_eq!(
            out,
            "[roles]\njudges = [\n  # head\n  # lead\n  \"b\",  # why b\n  \"c\",\n  \"a\",  # why a\n]  # tail\n"
        );
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
    fn fixer_is_a_string_for_one_and_an_array_for_a_chain() {
        let one = patch_role("", "fixer", &ids(&["a"])).unwrap();
        assert!(one.contains("fixer = \"a\""), "{one}");
        let two = patch_role("[roles]\nfixer = \"a\"\n", "fixer", &ids(&["a", "b"])).unwrap();
        assert!(two.contains("fixer = [\"a\", \"b\"]"), "{two}");
        let reset = patch_role(&two, "fixer", &[]).unwrap();
        assert!(!reset.contains("fixer ="), "{reset}");
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

    /// A current repo whose config names `roots`, and a second checkout under
    /// it whose own `magi.toml` is `other_toml`.
    fn two_repos(tmp: &TempDir, other_toml: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = tmp.path().join("ghq");
        let repo = root.join("h").join("o").join("cur");
        let other = root.join("h").join("o").join("other");
        for d in [&repo, &other] {
            std::fs::create_dir_all(d.join(".git")).unwrap();
        }
        std::fs::write(
            repo.join("magi.toml"),
            format!(
                "{AGENTS}\n[repos]\nroots = [{}]\n",
                quote(&root.to_string_lossy())
            ),
        )
        .unwrap();
        std::fs::write(other.join("magi.toml"), other_toml).unwrap();
        let machine = tmp.path().join("m").join("magi").join("config.toml");
        (repo, other, machine)
    }

    #[test]
    fn saving_judges_is_refused_when_another_repo_declares_them() {
        let tmp = TempDir::new().unwrap();
        let (repo, other, machine) =
            two_repos(&tmp, &format!("{AGENTS}\n[roles]\njudges = [\"a\"]\n"));
        let v = view(&repo, Some(&machine));
        let j = v.roles.iter().find(|r| r.key == "judges").unwrap();
        assert!(!j.editable, "{:?}", j.locked_reason);
        let err = save(
            &repo,
            Some(&machine),
            &v.revision,
            &BTreeMap::from([("judges".to_owned(), ids(&["b"]))]),
        )
        .unwrap_err();
        assert!(
            matches!(&err, SaveError::Refused(m) if m.contains("o/other")),
            "{err:?}"
        );
        assert!(!machine.exists());
        assert!(Config::load_layers(&repo_layers(&other)).is_ok());
        // A key nobody else declares still saves, and the other repo loads.
        save(
            &repo,
            Some(&machine),
            &v.revision,
            &BTreeMap::from([("reviewers".to_owned(), ids(&["b"]))]),
        )
        .unwrap();
        let mut layers = vec![machine.clone()];
        layers.extend(repo_layers(&other));
        assert!(Config::load_layers(&layers).is_ok());
    }

    #[test]
    fn saving_a_role_writes_no_agents_and_detection_stays_dynamic() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("magi.toml"), "[graph]\ncandidates = 1\n").unwrap();
        let machine = tmp.path().join("m").join("magi").join("config.toml");
        let v = view(&repo, Some(&machine));
        let Some(first) = Config::autodetected().agents.first().map(|a| a.id.clone()) else {
            return; // no agent CLI on PATH here; nothing to pick
        };
        save(
            &repo,
            Some(&machine),
            &v.revision,
            &BTreeMap::from([("judges".to_owned(), ids(&[&first]))]),
        )
        .unwrap();
        let text = std::fs::read_to_string(&machine).unwrap();
        assert!(!text.contains("[[agents]]"), "{text}");
        let after = view(&repo, Some(&machine));
        assert!(after.agents.iter().all(|a| a.source == "detected"));
        assert_eq!(
            ids_of(
                &Config::load_layers(&layer_paths(&repo, Some(&machine)))
                    .unwrap()
                    .agents
            ),
            ids_of(&Config::autodetected().agents)
        );
    }

    #[test]
    fn an_explicit_empty_agents_list_is_kept() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("magi.toml");
        std::fs::write(&f, "agents = []\n").unwrap();
        assert!(Config::load_layers(&[f]).unwrap().agents.is_empty());
    }

    #[test]
    fn the_repo_lock_says_the_repo_overrides_the_key() {
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
        let why = j.locked_reason.as_deref().unwrap();
        assert!(
            why.starts_with("This repo overrides roles.judges in "),
            "{why}"
        );
    }
}
