//! Magi ids in prose, resolved to the page that shows them.
//!
//! An agent's chat reply, a task's note or a notice names runs, tasks,
//! questions and conversations by id (`6a06`, `20260908-205802-c9eb`,
//! `chat@6f0d`). This module is the one place that decides whether such a token
//! is *real*: it is linked only when it names something that exists, so an
//! ordinary four-letter hex word (`beef`, `face`) stays text.
//!
//! - [`Index`] holds the ids of the four stores. A full id resolves when
//!   exactly one thing has it; a bare short id (the last four hex digits)
//!   resolves only when exactly one thing across all four stores ends in it;
//!   `kind@short` (the label a task's source carries) resolves within that
//!   kind only.
//! - [`href`] is the hash route rule, shared with the web layer's
//!   `source_link`.
//! - [`scan`] is the lexical rule. `assets/ui/app.js` repeats it (`ID_RE`) to
//!   cut plain text, and asks [`Index::table`] whether a token is real, so the
//!   decision stays here. Change both together.
//! - [`link_nodes`] applies it to a parsed markdown tree.

use std::collections::HashMap;

use serde::Serialize;

use crate::md::Node;

/// What an id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A queue task: `#/tasks/<id>`.
    Task,
    /// A run: `#/runs/<id>`.
    Run,
    /// A chat conversation: `#/chat/<id>`.
    Chat,
    /// A question or merge approval: `#/questions/<id>`.
    Question,
}

impl Kind {
    fn route(self) -> &'static str {
        match self {
            Self::Task => "tasks",
            Self::Run => "runs",
            Self::Chat => "chat",
            Self::Question => "questions",
        }
    }

    /// The kind a `qualifier@short` token names. A task's source label is
    /// `<node>@<short>` where the node is a run's node, so anything that is
    /// not a known kind word is a run.
    fn of_qualifier(word: &str) -> Self {
        match word {
            "chat" => Self::Chat,
            "task" => Self::Task,
            "question" | "ask" | "approval" => Self::Question,
            _ => Self::Run,
        }
    }
}

/// Percent-encode everything outside the URL-unreserved set.
pub fn encode_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The hash route that opens `id` of `kind`. The one place the rule lives.
pub fn href(kind: Kind, id: &str) -> String {
    format!("#/{}/{}", kind.route(), encode_segment(id))
}

/// A resolved id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Ref {
    /// What it names.
    pub kind: Kind,
    /// The full id.
    pub id: String,
    /// The hash route that opens it.
    pub href: String,
}

impl Ref {
    fn new(kind: Kind, id: &str) -> Self {
        Self {
            kind,
            id: id.to_owned(),
            href: href(kind, id),
        }
    }
}

/// A token found in text: byte range and the token itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Start byte.
    pub start: usize,
    /// End byte (exclusive).
    pub end: usize,
}

/// `YYYYMMDD-HHMMSS-xxxx`, the shape of every id magi mints; returns its length.
fn full_len(b: &[u8]) -> Option<usize> {
    let ok = b.len() >= 20
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'-'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'-'
        && b[16..20].iter().all(is_hex);
    ok.then_some(20)
}

fn is_hex(c: &u8) -> bool {
    c.is_ascii_digit() || (b'a'..=b'f').contains(c)
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')
}

/// Every candidate token in `text`: an optional `qualifier@`, then a full id
/// or four lowercase hex digits. Boundaries are ASCII-only on purpose, so an
/// id next to Japanese text (`12ba の修正`, `12baの修正`) still counts, while
/// the middle of a longer word, a path, a version or a file name does not.
pub fn scan(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let before_ok = i == 0 || !(word_byte(b[i - 1]) || matches!(b[i - 1], b'@' | b'.' | b'/'));
        if !before_ok || !(b[i].is_ascii_alphanumeric()) {
            i += 1;
            continue;
        }
        let mut j = i;
        // optional qualifier: [a-z][a-z-]*@
        if b[j].is_ascii_lowercase() {
            let mut k = j;
            while k < b.len() && (b[k].is_ascii_lowercase() || b[k] == b'-') {
                k += 1;
            }
            if k < b.len() && b[k] == b'@' {
                j = k + 1;
            }
        }
        let body = if j < b.len() {
            full_len(&b[j..])
                .or_else(|| (b.len() - j >= 4 && b[j..j + 4].iter().all(is_hex)).then_some(4))
        } else {
            None
        };
        let Some(n) = body else {
            i = skip_word(b, i);
            continue;
        };
        let end = j + n;
        let after_ok = end >= b.len()
            || !(word_byte(b[end])
                || (b[end] == b'.' && b.get(end + 1).is_some_and(u8::is_ascii_alphanumeric)));
        if after_ok {
            out.push(Span { start: i, end });
            i = end;
        } else {
            i = skip_word(b, i);
        }
    }
    out
}

/// Past the word starting at `i`, always by at least one byte.
fn skip_word(b: &[u8], mut i: usize) -> usize {
    i += 1;
    while i < b.len() && word_byte(b[i]) {
        i += 1;
    }
    i
}

/// The ids that exist.
#[derive(Debug, Clone, Default)]
pub struct Index {
    /// Full id -> kind; `None` when two stores share the id.
    full: HashMap<String, Option<Kind>>,
    /// Short id -> the one full id and kind; `None` when ambiguous.
    short: HashMap<String, Option<(Kind, String)>>,
    /// Per kind: short id -> full id; `None` when ambiguous within the kind.
    scoped: HashMap<(Kind, String), Option<String>>,
    /// Where the runs live, to read a run's finding ids on demand.
    runs_dir: Option<std::path::PathBuf>,
}

/// Merge `new` into a unique-or-ambiguous slot.
fn put<K: std::hash::Hash + Eq, V: PartialEq>(map: &mut HashMap<K, Option<V>>, key: K, new: V) {
    match map.get(&key) {
        None => {
            map.insert(key, Some(new));
        }
        Some(Some(old)) if *old == new => {}
        Some(_) => {
            map.insert(key, None);
        }
    }
}

fn short_of(id: &str) -> &str {
    id.rsplit('-').next().unwrap_or(id)
}

impl Index {
    /// An index over the given ids.
    pub fn new<'a>(
        tasks: impl IntoIterator<Item = &'a str>,
        runs: impl IntoIterator<Item = &'a str>,
        questions: impl IntoIterator<Item = &'a str>,
        talks: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let mut idx = Self::default();
        let groups: [(Kind, Vec<&str>); 4] = [
            (Kind::Task, tasks.into_iter().collect()),
            (Kind::Run, runs.into_iter().collect()),
            (Kind::Question, questions.into_iter().collect()),
            (Kind::Chat, talks.into_iter().collect()),
        ];
        for (kind, ids) in groups {
            for id in ids {
                idx.add(kind, id);
            }
        }
        idx
    }

    fn add(&mut self, kind: Kind, id: &str) {
        put(&mut self.full, id.to_owned(), kind);
        let short = short_of(id);
        // A full id is only worth indexing when it has the minted shape;
        // anything else resolves by its exact spelling alone.
        if short.len() == 4 && short.bytes().all(|c| is_hex(&c)) {
            put(&mut self.short, short.to_owned(), (kind, id.to_owned()));
            put(&mut self.scoped, (kind, short.to_owned()), id.to_owned());
        }
    }

    /// Read finding ids from `<dir>/<run>/run.json` when a document needs them.
    #[must_use]
    pub fn with_runs_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.runs_dir = Some(dir);
        self
    }

    /// The finding ids a run recorded, read tolerantly (any unreadable record
    /// has none).
    fn findings_of(&self, run: &str) -> std::collections::HashSet<String> {
        let Some(dir) = &self.runs_dir else {
            return Default::default();
        };
        let Ok(text) = std::fs::read_to_string(dir.join(run).join("run.json")) else {
            return Default::default();
        };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Default::default();
        };
        let list = |v: &serde_json::Value, key: &str| {
            v.get(key)
                .and_then(|x| x.as_array())
                .cloned()
                .unwrap_or_default()
        };
        list(&doc, "reviews")
            .iter()
            .flat_map(|round| list(round, "reviews"))
            .flat_map(|rec| list(&rec, "findings"))
            .filter_map(|f| f.get("id").and_then(|i| i.as_str()).map(str::to_owned))
            .collect()
    }

    /// The one real run `nodes` mention, when there is exactly one.
    fn single_run(&self, nodes: &[Node]) -> Option<String> {
        let mut texts = Vec::new();
        leaves(nodes, &mut texts);
        let mut found: Option<String> = None;
        for t in texts {
            for span in scan(t) {
                if let Some(r) = self.resolve(&t[span.start..span.end])
                    && r.kind == Kind::Run
                {
                    match &found {
                        Some(f) if *f != r.id => return None,
                        _ => found = Some(r.id),
                    }
                }
            }
        }
        found
    }

    /// Whether anything is indexed at all.
    pub fn is_empty(&self) -> bool {
        self.full.is_empty()
    }

    /// Resolve one token (as cut by [`scan`]) to something real, or `None`.
    pub fn resolve(&self, token: &str) -> Option<Ref> {
        if let Some((qualifier, short)) = token.split_once('@') {
            let kind = Kind::of_qualifier(qualifier);
            let full = self.scoped.get(&(kind, short.to_owned()))?.as_ref()?;
            return Some(Ref::new(kind, full));
        }
        if token.len() == 4 {
            let (kind, id) = self.short.get(token)?.as_ref()?;
            return Some(Ref::new(*kind, id));
        }
        let kind = (*self.full.get(token)?)?;
        Some(Ref::new(kind, token))
    }

    /// The same decisions as plain data, for the page to look tokens up in:
    /// `full`, `short`, and `scoped.<kind>` (what `kind@short` resolves to).
    pub fn table(&self) -> serde_json::Value {
        let mut full = serde_json::Map::new();
        for (id, kind) in &self.full {
            if let Some(kind) = kind {
                full.insert(id.clone(), serde_json::json!(Ref::new(*kind, id)));
            }
        }
        let mut short = serde_json::Map::new();
        for (s, hit) in &self.short {
            if let Some((kind, id)) = hit {
                short.insert(s.clone(), serde_json::json!(Ref::new(*kind, id)));
            }
        }
        let mut scoped = serde_json::Map::new();
        for ((kind, s), id) in &self.scoped {
            if let Some(id) = id {
                let slot = scoped
                    .entry(format!("{kind:?}").to_lowercase())
                    .or_insert_with(|| serde_json::json!({}));
                slot[s] = serde_json::json!(Ref::new(*kind, id));
            }
        }
        serde_json::json!({ "full": full, "short": short, "scoped": scoped })
    }
}

/// `R<round>-<reviewer>-<n>`, the shape magi gives a finding.
fn is_finding(token: &str) -> bool {
    let parts = token
        .strip_prefix('R')
        .into_iter()
        .flat_map(|r| r.split('-'));
    let mut n = 0;
    for p in parts {
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        n += 1;
    }
    n == 3
}

/// Finding-shaped tokens in `text`, with the same ASCII boundaries as [`scan`].
fn scan_findings(text: &str) -> Vec<Span> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let before_ok = i == 0 || !(word_byte(b[i - 1]) || matches!(b[i - 1], b'@' | b'.' | b'/'));
        if before_ok && b[i] == b'R' {
            let mut end = i + 1;
            while end < b.len() && (b[end].is_ascii_digit() || b[end] == b'-') {
                end += 1;
            }
            let after_ok = end >= b.len() || !word_byte(b[end]);
            if after_ok && is_finding(&text[i..end]) {
                out.push(Span { start: i, end });
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// One pass of linking over a document. A finding id repeats across runs, so
/// it resolves only when the document pins down one run (the caller names it,
/// or exactly one real run is mentioned) and that run recorded the finding.
struct Linker<'a> {
    idx: &'a Index,
    run: Option<String>,
    findings: std::cell::OnceCell<std::collections::HashSet<String>>,
}

impl<'a> Linker<'a> {
    fn new(idx: &'a Index, nodes: &[Node], run: Option<&str>) -> Self {
        let run = run.map(str::to_owned).or_else(|| idx.single_run(nodes));
        Self {
            idx,
            run,
            findings: std::cell::OnceCell::new(),
        }
    }

    fn resolve(&self, token: &str) -> Option<Ref> {
        if is_finding(token) {
            let run = self.run.as_deref()?;
            let known = self.findings.get_or_init(|| self.idx.findings_of(run));
            return known.contains(token).then(|| Ref {
                kind: Kind::Run,
                id: run.to_owned(),
                href: format!("{}/report", href(Kind::Run, run)),
            });
        }
        self.idx.resolve(token)
    }

    /// Split `text` into plain pieces and resolved refs, in order.
    fn split<'t>(&self, text: &'t str) -> Vec<Result<&'t str, (&'t str, Ref)>> {
        let mut spans = scan(text);
        spans.extend(scan_findings(text));
        spans.sort_by_key(|s| s.start);
        let mut out = Vec::new();
        let mut last = 0;
        for span in spans {
            let token = &text[span.start..span.end];
            if let Some(r) = self.resolve(token) {
                if span.start > last {
                    out.push(Ok(&text[last..span.start]));
                }
                out.push(Err((token, r)));
                last = span.end;
            }
        }
        if last < text.len() {
            out.push(Ok(&text[last..]));
        }
        out
    }

    fn nodes(&self, nodes: Vec<Node>) -> Vec<Node> {
        nodes.into_iter().flat_map(|n| self.node(n)).collect()
    }

    fn node(&self, node: Node) -> Vec<Node> {
        let kids = |v: Vec<Node>| self.nodes(v);
        vec![match node {
            Node::Text { value } => {
                let pieces = self.split(&value);
                if pieces.iter().all(Result::is_ok) {
                    return vec![Node::Text { value }];
                }
                return pieces
                    .into_iter()
                    .map(|p| match p {
                        Ok(t) => Node::Text {
                            value: t.to_owned(),
                        },
                        Err((t, r)) => Node::Ref {
                            kind: r.kind,
                            href: r.href,
                            text: t.to_owned(),
                            code: false,
                        },
                    })
                    .collect();
            }
            Node::Code { code } => {
                let pieces = self.split(&code);
                if pieces.iter().all(Result::is_ok) {
                    return vec![Node::Code { code }];
                }
                return pieces
                    .into_iter()
                    .map(|p| match p {
                        Ok(t) => Node::Code { code: t.to_owned() },
                        Err((t, r)) => Node::Ref {
                            kind: r.kind,
                            href: r.href,
                            text: t.to_owned(),
                            code: true,
                        },
                    })
                    .collect();
            }
            Node::Paragraph { children } => Node::Paragraph {
                children: kids(children),
            },
            Node::Heading { level, children } => Node::Heading {
                level,
                children: kids(children),
            },
            Node::BulletList { items } => Node::BulletList { items: kids(items) },
            Node::OrderedList { start, items } => Node::OrderedList {
                start,
                items: kids(items),
            },
            Node::ListItem { checked, children } => Node::ListItem {
                checked,
                children: kids(children),
            },
            Node::Table { align, rows } => Node::Table {
                align,
                rows: rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|mut c| {
                                c.children = kids(c.children);
                                c
                            })
                            .collect()
                    })
                    .collect(),
            },
            Node::BlockQuote { children } => Node::BlockQuote {
                children: kids(children),
            },
            Node::Emphasis { children } => Node::Emphasis {
                children: kids(children),
            },
            Node::Strong { children } => Node::Strong {
                children: kids(children),
            },
            Node::Strikethrough { children } => Node::Strikethrough {
                children: kids(children),
            },
            // A link keeps its text as written: an anchor inside an anchor is
            // invalid, and the author chose where it points.
            other => other,
        }]
    }
}

/// The text and inline-code leaves of a tree, outside links.
fn leaves<'a>(nodes: &'a [Node], out: &mut Vec<&'a str>) {
    for n in nodes {
        match n {
            Node::Text { value } => out.push(value),
            Node::Code { code } => out.push(code),
            Node::Paragraph { children }
            | Node::Heading { children, .. }
            | Node::ListItem { children, .. }
            | Node::BlockQuote { children }
            | Node::Emphasis { children }
            | Node::Strong { children }
            | Node::Strikethrough { children } => leaves(children, out),
            Node::BulletList { items } | Node::OrderedList { items, .. } => leaves(items, out),
            Node::Table { rows, .. } => {
                rows.iter().flatten().for_each(|c| leaves(&c.children, out))
            }
            _ => {}
        }
    }
}

/// Turn the real ids in `nodes` into [`Node::Ref`]. Text and inline code are
/// searched; code blocks, images and anything under an existing link are left
/// alone. A finding id links when the document mentions exactly one real run.
pub fn link_nodes(nodes: Vec<Node>, idx: &Index) -> Vec<Node> {
    if idx.is_empty() {
        return nodes;
    }
    let linker = Linker::new(idx, &nodes, None);
    linker.nodes(nodes)
}

/// As [`link_nodes`], for prose that belongs to `run` (its own page).
pub fn link_nodes_in(nodes: Vec<Node>, idx: &Index, run: &str) -> Vec<Node> {
    if idx.is_empty() {
        return nodes;
    }
    let linker = Linker::new(idx, &nodes, Some(run));
    linker.nodes(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::md::{ImageBase, to_nodes};

    const RUN: &str = "20260908-205802-c9eb";
    const TASK: &str = "20260909-010101-d678";
    const CHAT: &str = "20260904-014455-6f0d";
    const Q: &str = "20260910-111111-12ba";

    fn idx() -> Index {
        Index::new([TASK], [RUN], [Q], [CHAT])
    }

    fn tokens(text: &str) -> Vec<&str> {
        scan(text).iter().map(|s| &text[s.start..s.end]).collect()
    }

    #[test]
    fn scan_cuts_ids_and_respects_boundaries() {
        assert_eq!(tokens("task d678 and 12ba の修正"), ["d678", "12ba"]);
        assert_eq!(tokens("12baの修正を待つ"), ["12ba"]);
        assert_eq!(tokens("source chat@6f0d."), ["chat@6f0d"]);
        assert_eq!(tokens(&format!("run {RUN}!")), [RUN]);
        // Inside a longer word, a path, a file name or a number: not a token.
        assert!(tokens("deadbeef abcde0 foo-beef beef.rs /beef v1.beef").is_empty());
    }

    #[test]
    fn only_real_ids_resolve() {
        let idx = idx();
        assert_eq!(idx.resolve("d678").unwrap().href, format!("#/tasks/{TASK}"));
        assert_eq!(idx.resolve("c9eb").unwrap().kind, Kind::Run);
        assert_eq!(
            idx.resolve("12ba").unwrap().href,
            format!("#/questions/{Q}")
        );
        assert_eq!(idx.resolve(CHAT).unwrap().href, format!("#/chat/{CHAT}"));
        assert_eq!(idx.resolve("chat@6f0d").unwrap().id, CHAT);
        assert_eq!(idx.resolve("implement@c9eb").unwrap().kind, Kind::Run);
        assert!(idx.resolve("beef").is_none());
        assert!(idx.resolve("face").is_none());
        // A qualifier limits the search to its kind.
        assert!(idx.resolve("chat@c9eb").is_none());
    }

    #[test]
    fn a_short_id_shared_by_two_things_resolves_by_full_id_only() {
        let a = "20260101-000000-aaaa";
        let b = "20260202-000000-aaaa";
        let idx = Index::new([a], [b], [], []);
        assert!(idx.resolve("aaaa").is_none());
        assert_eq!(idx.resolve(a).unwrap().kind, Kind::Task);
        assert_eq!(idx.resolve("task@aaaa").unwrap().id, a);
        assert_eq!(idx.resolve("run@aaaa").unwrap().id, b);
        assert!(idx.table()["short"].get("aaaa").is_none());
    }

    #[test]
    fn markdown_text_and_inline_code_link_but_blocks_and_links_do_not() {
        let md = format!(
            "see `{RUN}` and d678, not beef.\n\n```\nd678\n```\n\n[d678](https://example.com) `x d678 c9eb`\n"
        );
        let nodes = link_nodes(to_nodes(&md, &ImageBase::None), &idx());
        let json = serde_json::to_string(&nodes).unwrap();
        assert!(
            json.contains(&format!(
                r##""type":"ref","kind":"run","href":"#/runs/{RUN}","text":"{RUN}","code":true"##
            )),
            "{json}"
        );
        assert!(json.contains(r#""kind":"task""#), "{json}");
        assert!(!json.contains(r#""text":"beef""#), "{json}");
        // The code block and the explicit link are untouched.
        assert!(
            json.contains(r#"{"type":"code_block","lang":null,"code":"d678\n"}"#),
            "{json}"
        );
        assert_eq!(json.matches(r#""type":"ref""#).count(), 4, "{json}");
    }

    #[test]
    fn no_index_changes_nothing() {
        let nodes = to_nodes("d678", &ImageBase::None);
        assert_eq!(link_nodes(nodes.clone(), &Index::default()), nodes);
    }

    #[test]
    fn href_encodes_the_segment() {
        assert_eq!(href(Kind::Chat, "a b"), "#/chat/a%20b");
    }

    #[test]
    fn a_finding_links_only_when_one_run_is_pinned_and_recorded_it() {
        let dir = std::env::temp_dir().join(format!("idref-findings-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(RUN)).unwrap();
        std::fs::write(
            dir.join(RUN).join("run.json"),
            r#"{"reviews":[{"reviews":[{"findings":[{"id":"R1-1-1"}]}]}]}"#,
        )
        .unwrap();
        let idx = idx().with_runs_dir(dir.clone());
        let link = |md: &str| {
            serde_json::to_string(&link_nodes(to_nodes(md, &ImageBase::None), &idx)).unwrap()
        };
        let hit = link("run c9eb found `R1-1-1` and R9-9-9");
        assert!(hit.contains(&format!("#/runs/{RUN}/report")), "{hit}");
        assert_eq!(hit.matches(r#""type":"ref""#).count(), 2, "{hit}");
        // No run in the text: nothing to pin the finding to.
        assert!(!link("R1-1-1").contains(r#""type":"ref""#));
        // Two runs: ambiguous.
        let two = Index::new([], [RUN, "20260101-000000-aaaa"], [], []).with_runs_dir(dir.clone());
        let out = serde_json::to_string(&link_nodes(
            to_nodes("c9eb aaaa R1-1-1", &ImageBase::None),
            &two,
        ))
        .unwrap();
        assert_eq!(out.matches(r#""type":"ref""#).count(), 2, "{out}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
