//! Store-backed references shared by Markdown and plain-text UI surfaces.
use super::*;

#[derive(Debug, Clone, Default)]
pub(super) struct IdIndex {
    entries: Vec<Entry>,
}
#[derive(Debug, Clone)]
struct Entry {
    kind: &'static str,
    id: String,
    href: String,
    record: Option<Arc<RecordValidation>>,
}

#[derive(Debug)]
struct RecordValidation {
    root: PathBuf,
    valid: std::sync::OnceLock<bool>,
}

impl Entry {
    fn valid(&self) -> bool {
        self.record.as_ref().is_none_or(|record| {
            *record.valid.get_or_init(|| match self.kind {
                "run" => read_run(&record.root, &self.id).is_ok(),
                "task" => Queue::at(record.root.clone()).get(&self.id).is_ok(),
                "question" => Questions::at(record.root.clone()).get(&self.id).is_ok(),
                "chat" => Talks::at(record.root.clone()).get(&self.id).is_ok(),
                _ => false,
            })
        })
    }
}

#[derive(Deserialize)]
pub(super) struct TextRequest {
    text: String,
    #[serde(default)]
    code: bool,
}
#[derive(Serialize, Debug, PartialEq)]
pub(super) struct Part {
    text: String,
    href: Option<String>,
}

/// The client watches only ranges that can change when a store revision moves.
/// Keep candidate recognition here alongside the resolver, not in client regexes.
#[derive(Serialize)]
pub(super) struct TextResolution {
    parts: Vec<Part>,
    watch: bool,
}

impl IdIndex {
    fn text_resolution(&self, text: &str, code: bool) -> TextResolution {
        TextResolution {
            parts: self.parts(text, code),
            watch: needs_index(text),
        }
    }
}

pub(super) fn page_href(route: &str, id: &str) -> String {
    format!("#/{route}/{}", encode_segment(id))
}

impl IdIndex {
    fn from_stamps(
        roots: &[PathBuf; 4],
        stores: &[Stamps; 4],
        previous: Option<&Snapshot>,
    ) -> Self {
        let prior: HashMap<_, _> = previous
            .into_iter()
            .flat_map(|s| &s.ordinary.entries)
            .map(|entry| ((entry.kind, entry.id.as_str()), entry))
            .collect();
        let mut index = Self::default();
        for (i, ((kind, route), ids)) in [
            ("run", "runs"),
            ("task", "tasks"),
            ("question", "questions"),
            ("chat", "chat"),
        ]
        .into_iter()
        .zip(stores)
        .enumerate()
        {
            for (id, stamp) in ids {
                // A filename supplies a candidate. Only referenced candidates are
                // decoded, once per file revision, using the destination's reader.
                let record = previous
                    .filter(|s| s.roots[i] == roots[i] && s.stores[i].get(id) == Some(stamp))
                    .and_then(|_| prior.get(&(kind, id.as_str())))
                    .and_then(|entry| entry.record.clone())
                    .unwrap_or_else(|| {
                        Arc::new(RecordValidation {
                            root: roots[i].clone(),
                            valid: std::sync::OnceLock::new(),
                        })
                    });
                index.entries.push(Entry {
                    kind,
                    id: id.clone(),
                    href: page_href(route, id),
                    record: Some(record),
                });
            }
        }
        index
    }

    // Review history is only needed when the input contains finding references.
    pub(super) fn findings(&mut self, runs: &FsPath) {
        for id in run_ids(runs) {
            if let Ok(run) = read_run(runs, &id) {
                for finding in run
                    .reviews
                    .iter()
                    .flat_map(|r| &r.reviews)
                    .flat_map(|r| &r.findings)
                {
                    self.add(
                        "finding",
                        &finding.id,
                        format!("{}/report", page_href("runs", &id)),
                    );
                }
            }
        }
    }

    fn add(&mut self, kind: &'static str, id: &str, href: String) {
        self.entries.push(Entry {
            kind,
            id: id.to_owned(),
            href,
            record: None, // Findings were decoded with their run; unit fixtures are trusted.
        });
    }

    fn resolve(&self, token: &str, _code: bool) -> Option<String> {
        let (kind, id) = match token.split_once('@') {
            Some(("run", id)) => (Some("run"), id),
            Some(("task", id)) => (Some("task"), id),
            Some(("chat", id)) => (Some("chat"), id),
            Some(("question" | "approval" | "merge-approval", id)) => (Some("question"), id),
            Some(_) => return None,
            None => (None, token),
        };
        let short = id.len() == 4 && id.bytes().all(|b| b.is_ascii_hexdigit());
        let eligible = |e: &&Entry| kind.is_none_or(|kind| e.kind == kind);
        let exact: Vec<_> = self
            .entries
            .iter()
            .filter(eligible)
            .filter(|e| e.id == id)
            .filter(|e| e.valid())
            .collect();
        let matches = if exact.is_empty() && short {
            self.entries
                .iter()
                .filter(eligible)
                .filter(|e| crate::queue::short(&e.id) == id)
                .filter(|e| e.valid())
                .collect()
        } else {
            exact
        };
        let first = matches.first()?;
        matches
            .iter()
            .all(|e| e.href == first.href)
            .then(|| first.href.clone())
    }

    fn parts(&self, text: &str, code: bool) -> Vec<Part> {
        let mut parts = Vec::new();
        let mut end = 0;
        let mut pos = 0;
        // ASCII tokens make Japanese punctuation a boundary, while identifiers,
        // paths and URLs cannot accidentally expose an embedded short id.
        let bytes = text.as_bytes();
        while pos < bytes.len() {
            if text[pos..].starts_with("https://") || text[pos..].starts_with("http://") {
                pos += text[pos..]
                    .find(char::is_whitespace)
                    .unwrap_or(text.len() - pos);
                continue;
            }
            let ch = text[pos..].chars().next().expect("character boundary");
            if !ch.is_ascii_alphanumeric() {
                pos += ch.len_utf8();
                continue;
            }
            let start = pos;
            while pos < bytes.len()
                && (bytes[pos].is_ascii_alphanumeric()
                    || matches!(bytes[pos], b'-' | b'_' | b'@' | b'/' | b'.'))
            {
                pos += 1;
            }
            // A sentence's full stop is punctuation, not part of the id.
            let token = text[start..pos].trim_end_matches('.');
            let embedded =
                start > 0 && matches!(bytes[start - 1], b'/' | b'_' | b'-' | b'@' | b'.');
            if let Some(href) = (!embedded).then(|| self.resolve(token, code)).flatten() {
                if start > end {
                    parts.push(Part {
                        text: text[end..start].into(),
                        href: None,
                    });
                }
                parts.push(Part {
                    text: token.into(),
                    href: Some(href),
                });
                end = start + token.len();
            }
        }
        if end < text.len() {
            parts.push(Part {
                text: text[end..].into(),
                href: None,
            });
        }
        parts
    }

    pub(super) fn markdown(&self, nodes: &mut Vec<md::Node>) {
        use md::Node;
        let mut linked = Vec::new();
        for mut node in std::mem::take(nodes) {
            match &mut node {
                Node::Text { .. } | Node::Code { .. } => {
                    let code = matches!(node, Node::Code { .. });
                    let value = match &node {
                        Node::Text { value } => value,
                        Node::Code { code } => code,
                        _ => unreachable!(),
                    };
                    for part in self.parts(value, code) {
                        let leaf = if code {
                            Node::Code { code: part.text }
                        } else {
                            Node::Text { value: part.text }
                        };
                        linked.push(if let Some(href) = part.href {
                            Node::InternalLink {
                                href,
                                children: vec![leaf],
                            }
                        } else {
                            leaf
                        });
                    }
                    continue;
                }
                Node::Paragraph { children }
                | Node::Heading { children, .. }
                | Node::ListItem { children, .. }
                | Node::BlockQuote { children }
                | Node::Emphasis { children }
                | Node::Strong { children }
                | Node::Strikethrough { children } => self.markdown(children),
                Node::BulletList { items } | Node::OrderedList { items, .. } => {
                    self.markdown(items)
                }
                Node::Table { rows, .. } => {
                    for row in rows {
                        for cell in row {
                            self.markdown(&mut cell.children);
                        }
                    }
                }
                // Existing links, images and code blocks are deliberately opaque.
                _ => {}
            }
            linked.push(node);
        }
        *nodes = linked;
    }
}

pub(super) async fn resolve_text(
    State(ui): State<Arc<Ui>>,
    Json(texts): Json<Vec<TextRequest>>,
) -> ApiResult<Json<Vec<TextResolution>>> {
    if texts.len() > 64 || texts.iter().map(|t| t.text.len()).sum::<usize>() > 65_536 {
        return Err(ApiError::bad_request("reference batch too large"));
    }
    blocking(move || {
        let index = if texts.iter().any(|t| needs_index(&t.text)) {
            ui.references.get(
                &ui.runs,
                &ui.queue,
                &ui.questions,
                &ui.talks,
                texts.iter().any(|t| has_finding_reference(&t.text)),
            )
        } else {
            Arc::new(IdIndex::default())
        };
        Ok(Json(
            texts
                .iter()
                .map(|t| index.text_resolution(&t.text, t.code))
                .collect(),
        ))
    })
    .await
}

/// Only a complete finding token demands review history, never a stray `R`.
fn is_finding_reference(token: &str) -> bool {
    let Some(rest) = token.strip_prefix('R') else {
        return false;
    };
    let parts: Vec<_> = rest.split('-').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '@' | '/' | '.'))
        .map(|token| token.trim_end_matches('.'))
}

fn has_finding_reference(text: &str) -> bool {
    tokens(text).any(is_finding_reference)
}

fn needs_index(text: &str) -> bool {
    tokens(text).any(|token| {
        let id = token.rsplit('@').next().unwrap_or(token);
        is_finding_reference(id)
            || (id.len() == 4 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            || {
                let parts: Vec<_> = id.split('-').collect();
                parts.len() == 3
                    && parts[0].len() == 8
                    && parts[1].len() == 6
                    && parts[2].len() == 4
                    && parts[0]
                        .bytes()
                        .chain(parts[1].bytes())
                        .all(|b| b.is_ascii_digit())
                    && parts[2].bytes().all(|b| b.is_ascii_hexdigit())
            }
    })
}

fn talk_texts(talk: &Talk) -> impl Iterator<Item = &str> {
    talk.turns.iter().map(|t| t.body.as_str()).chain(
        talk.turns
            .iter()
            .flat_map(|t| &t.messages)
            .chain(&talk.pending_messages)
            .flat_map(|m| {
                std::iter::once(m.body.as_str()).chain(m.display.as_ref().map(|d| d.facts.as_str()))
            }),
    )
}

pub(super) fn has_findings(talk: &Talk) -> bool {
    talk_texts(talk).any(has_finding_reference)
}
pub(super) fn has_references(talk: &Talk) -> bool {
    talk_texts(talk).any(needs_index)
}

fn cache_lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Owned by one UI, shared by its clones. Only metadata is inspected on cache
/// hits. Candidate records are decoded only when referenced, and validation
/// results survive unrelated store updates. Review history is also lazy.
#[derive(Debug, Default)]
pub(super) struct IndexCache {
    snapshot: Mutex<Option<Snapshot>>,
    reviews: Mutex<Option<ReviewSnapshot>>,
}
#[derive(Debug)]
struct Snapshot {
    roots: [PathBuf; 4],
    stores: [Stamps; 4],
    ordinary: Arc<IdIndex>,
    findings: Option<Arc<IdIndex>>,
}
#[derive(Debug)]
struct ReviewSnapshot {
    root: PathBuf,
    stamps: Stamps,
    index: Arc<IdIndex>,
}
impl IndexCache {
    pub(super) fn get(
        &self,
        runs: &FsPath,
        queue: &Queue,
        questions: &Questions,
        talks: &Talks,
        findings: bool,
    ) -> Arc<IdIndex> {
        let roots = [
            runs.to_owned(),
            queue.root().to_owned(),
            questions.root().to_owned(),
            talks.root().to_owned(),
        ];
        let stores = std::array::from_fn(|i| store_stamps(&roots[i], i == 0));
        let mut slot = cache_lock(&self.snapshot);
        if slot
            .as_ref()
            .is_none_or(|s| s.roots != roots || s.stores != stores)
        {
            *slot = Some(Snapshot {
                ordinary: Arc::new(IdIndex::from_stamps(&roots, &stores, slot.as_ref())),
                roots,
                stores,
                findings: None,
            });
        }
        let snapshot = slot.as_mut().expect("initialized snapshot");
        if findings {
            Arc::clone(snapshot.findings.get_or_insert_with(|| {
                let mut reviews = cache_lock(&self.reviews);
                if reviews
                    .as_ref()
                    .is_none_or(|r| r.root != runs || r.stamps != snapshot.stores[0])
                {
                    let mut index = IdIndex::default();
                    index.findings(runs);
                    *reviews = Some(ReviewSnapshot {
                        root: runs.to_owned(),
                        stamps: snapshot.stores[0].clone(),
                        index: Arc::new(index),
                    });
                }
                let mut index = (*snapshot.ordinary).clone();
                index.entries.extend(
                    reviews
                        .as_ref()
                        .expect("initialized reviews")
                        .index
                        .entries
                        .iter()
                        .cloned(),
                );
                Arc::new(index)
            }))
        } else {
            Arc::clone(&snapshot.ordinary)
        }
    }

    pub(super) fn for_talk(&self, home: &FsPath, talk: &Talk) -> Arc<IdIndex> {
        if !has_references(talk) {
            return Arc::new(IdIndex::default());
        }
        self.get(
            &home.join("runs"),
            &Queue::at(home.join("queue")),
            &Questions::at(home.join("questions")),
            &Talks::at(home.join("talks")),
            has_findings(talk),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> IdIndex {
        let mut index = IdIndex::default();
        index.add(
            "run",
            "20260908-205802-6a06",
            page_href("runs", "20260908-205802-6a06"),
        );
        index.add(
            "task",
            "20260908-205803-12ba",
            page_href("tasks", "20260908-205803-12ba"),
        );
        index.add(
            "task",
            "20260908-205804-dead",
            page_href("tasks", "20260908-205804-dead"),
        );
        index.add(
            "question",
            "20260908-205805-d678",
            page_href("questions", "20260908-205805-d678"),
        );
        index.add(
            "chat",
            "20260908-205806-6f0d",
            page_href("chat", "20260908-205806-6f0d"),
        );
        index.add(
            "finding",
            "R2-1-3",
            "#/runs/20260908-205802-6a06/report".into(),
        );
        index
    }

    #[test]
    fn references_require_unique_real_destinations() {
        let mut index = index();
        assert!(index.resolve("ffff", true).is_none());
        assert!(index.resolve("dead", false).is_some());
        let parts = index.parts("dead の修正を待つ。beef は未登録", false);
        assert_eq!(
            parts
                .iter()
                .filter(|p| p.href.is_some())
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            ["dead"]
        );
        assert!(index.resolve("dead", true).is_some());
        assert!(index.resolve("task@dead", false).is_some());
        assert!(index.resolve("6a06", false).is_some());
        assert!(
            index
                .resolve("chat@6f0d", false)
                .unwrap()
                .starts_with("#/chat/")
        );
        assert!(
            index
                .resolve("approval@d678", false)
                .unwrap()
                .starts_with("#/questions/")
        );
        assert!(index.resolve("R2-1-3", false).unwrap().ends_with("/report"));
        index.add(
            "task",
            "20260909-000000-6a06",
            page_href("tasks", "20260909-000000-6a06"),
        );
        assert!(index.resolve("6a06", false).is_none());
        assert!(index.resolve("run@6a06", false).is_some());
        assert!(index.resolve("20260908-205802-6a06", false).is_some());
        index.add("finding", "R2-1-3", "#/runs/other/report".into());
        assert!(index.resolve("R2-1-3", false).is_none());
        index.add(
            "run",
            "20260909-000000-dead",
            page_href("runs", "20260909-000000-dead"),
        );
        assert!(index.resolve("dead", false).is_none());
        assert!(index.resolve("task@dead", false).is_some());
    }

    #[test]
    fn references_respect_boundaries_and_leave_urls_opaque() {
        let index = index();
        let text =
            "12baの修正。chat@6f0d, d678. abc12ba /tmp/12ba /12ba _12ba https://host/6a06 0000";
        let parts = index.parts(text, false);
        assert_eq!(
            parts.iter().map(|p| p.text.as_str()).collect::<String>(),
            text
        );
        assert_eq!(
            parts
                .iter()
                .filter(|p| p.href.is_some())
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            ["12ba", "chat@6f0d", "d678"]
        );
    }

    #[test]
    fn references_link_markdown_text_and_code_but_not_opaque_nodes() {
        let mut nodes = md::to_nodes(
            "6a06 **12ba** dead `dead 6a06` [6a06](https://example.org)\n\n```\n6a06\n```",
            &md::ImageBase::None,
        );
        index().markdown(&mut nodes);
        let json = serde_json::to_value(&nodes).unwrap();
        let encoded = json.to_string();
        assert_eq!(encoded.matches("internal_link").count(), 5);
        assert!(encoded.contains("code_block"));
        assert!(encoded.contains("https://example.org"));
        assert!(encoded.contains("\"type\":\"code\""));
    }

    #[test]
    fn references_load_only_existing_store_records() {
        let temp = tempfile::tempdir().unwrap();
        let queue = Queue::at(temp.path().join("queue"));
        let mut task = Task::new(
            "task".into(),
            "instruction".into(),
            temp.path().into(),
            Source::Human,
        );
        task.id = "20260908-205803-12ba".into();
        queue.put(&mut task).unwrap();
        let questions = Questions::at(temp.path().join("questions"));
        let mut question = ask::Question::new(
            "run".into(),
            "merge".into(),
            "operator".into(),
            "Approve?".into(),
            "detail".into(),
            Vec::new(),
        );
        question.id = "20260908-205805-d678".into();
        question.status = ask::QuestionStatus::Answered;
        questions.put(&mut question).unwrap();
        let runs = temp.path().join("runs");
        std::fs::create_dir_all(runs.join("20260908-205802-6a06")).unwrap();

        let cache = IndexCache::default();
        let index = cache.get(
            &runs,
            &queue,
            &questions,
            &Talks::at(temp.path().join("talks")),
            false,
        );
        assert!(index.resolve("12ba", false).is_some());
        assert!(index.resolve("6a06", false).is_none());
        assert!(
            index
                .resolve("d678", false)
                .unwrap()
                .starts_with("#/questions/")
        );
        std::fs::remove_file(queue.path_of(&task.id)).unwrap();
        let refreshed = cache.get(
            &runs,
            &queue,
            &questions,
            &Talks::at(temp.path().join("talks")),
            false,
        );
        assert!(refreshed.resolve("12ba", false).is_none());
    }
    #[test]
    fn reference_cache_reuses_metadata_indexes_and_invalidates_removed_ids() {
        let temp = tempfile::tempdir().unwrap();
        let runs = temp.path().join("runs");
        let queue = Queue::at(temp.path().join("queue"));
        let questions = Questions::at(temp.path().join("questions"));
        let talks = Talks::at(temp.path().join("talks"));
        let cache = IndexCache::default();
        let run_dir = runs.join("20260908-205802-6a06");
        std::fs::create_dir_all(&run_dir).unwrap();
        let metadata_dir = runs.join("20260908-205802-7b07");
        std::fs::create_dir_all(&metadata_dir).unwrap();
        std::fs::write(metadata_dir.join("run.json"), "metadata-only fixture").unwrap();
        let get = |findings| cache.get(&runs, &queue, &questions, &talks, findings);
        let mut run = RunState::new(
            temp.path().into(),
            "main".into(),
            "base".into(),
            "instruction".into(),
            Config::default(),
        );
        run.id = "20260908-205802-6a06".into();
        run.reviews = serde_json::from_value(serde_json::json!([{
            "round": 1, "head": "base", "reviews": [{ "reviewer": 1, "agent": "reviewer",
                "findings": [{ "id": "R2-1-3", "severity": "major", "title": "test finding" }] }]
        }]))
        .unwrap();
        run.save_under(temp.path()).unwrap();
        let first = get(false);
        assert!(first.resolve("6a06", false).is_some());
        let validated = |index: &IdIndex, id: &str| {
            Arc::clone(
                index
                    .entries
                    .iter()
                    .find(|e| e.id == id)
                    .unwrap()
                    .record
                    .as_ref()
                    .unwrap(),
            )
        };
        let invalid_record = validated(&first, "20260908-205802-7b07");
        assert!(invalid_record.valid.get().is_none());
        assert!(first.resolve("7b07", false).is_none());
        assert_eq!(invalid_record.valid.get(), Some(&false));
        assert!(Arc::ptr_eq(&first, &get(false)));
        assert!(cache_lock(&cache.reviews).is_none());
        assert!(first.resolve("R2-1-3", false).is_none());
        let reviewed = get(true);
        assert!(reviewed.resolve("R2-1-3", false).is_some());
        assert!(Arc::ptr_eq(&reviewed, &get(true)));
        let reviews = Arc::clone(&cache_lock(&cache.reviews).as_ref().unwrap().index);
        let mut task = Task::new(
            "target".into(),
            "instruction".into(),
            temp.path().into(),
            Source::Human,
        );
        task.id = "20260908-205803-dead".into();
        queue.put(&mut task).unwrap();
        let changed = get(false);
        assert!(!Arc::ptr_eq(&first, &changed));
        assert!(changed.resolve("dead", false).is_some());
        assert!(Arc::ptr_eq(
            &invalid_record,
            &validated(&changed, "20260908-205802-7b07")
        ));
        assert!(Arc::ptr_eq(
            &validated(&first, "20260908-205802-6a06"),
            &validated(&changed, "20260908-205802-6a06")
        ));
        assert_eq!(
            validated(&changed, "20260908-205802-6a06").valid.get(),
            Some(&true)
        );
        get(true);
        assert!(Arc::ptr_eq(
            &reviews,
            &cache_lock(&cache.reviews).as_ref().unwrap().index
        ));
        std::fs::remove_file(queue.path_of(&task.id)).unwrap();
        assert!(get(false).resolve("dead", false).is_none());
        std::fs::remove_file(run_dir.join("run.json")).unwrap();
        assert!(get(false).resolve("6a06", false).is_none());
        get(true);
        assert!(!Arc::ptr_eq(
            &reviews,
            &cache_lock(&cache.reviews).as_ref().unwrap().index
        ));
    }

    #[test]
    fn text_resolution_watches_only_candidate_ranges() {
        let index = IdIndex::default();
        for text in ["Reply is Ready", "ordinary prose", "R", "", "12345"] {
            let result = index.text_resolution(text, false);
            assert!(!result.watch, "{text}");
            assert!(result.parts.iter().all(|p| p.href.is_none()));
        }
        // Missing candidates still need watching: a later store revision can
        // create their destinations. Alphabetic hex and source labels count too.
        for text in [
            "12ba",
            "dead",
            "chat@6f0d",
            "20260908-205802-c9eb",
            "R2-1-3",
        ] {
            for code in [false, true] {
                assert!(index.text_resolution(text, code).watch, "{text}");
            }
        }
    }

    #[test]
    fn reference_index_and_review_loading_require_complete_candidates() {
        for text in [
            "Reply is Ready",
            "R",
            "R2",
            "R2-1",
            "xR2-1-3",
            "R2-1-3suffix",
        ] {
            assert!(!has_finding_reference(text), "{text}");
            assert!(!needs_index(text), "{text}");
        }
        for text in ["R2-1-3", "(R12-34-56).", "R2-1-3の修正"] {
            assert!(has_finding_reference(text), "{text}");
            assert!(needs_index(text), "{text}");
        }
        for text in ["dead の修正を待つ", "chat@6f0d", "20260908-205802-c9eb"] {
            assert!(needs_index(text), "{text}");
            assert!(!has_finding_reference(text), "{text}");
        }
    }
    #[test]
    fn talk_views_build_the_shared_index_lazily() {
        let home = tempfile::tempdir().unwrap();
        let mut talk: Talk = serde_json::from_value(serde_json::json!({
            "schema": 1, "id": "20260908-205806-6f0d", "repo": ".", "agent": "test", "status": "open",
            "turns": [], "created_at": "2026-09-08T20:58:02Z", "updated_at": "2026-09-08T20:58:02Z",
            "seat": crate::agent::SeatState::new("chat", "test", 1)
        })).unwrap();
        talk.turns.push(talk::Turn {
            sender: None,
            who: talk::Who::Agent,
            body: "Reply is Ready".into(),
            at: talk.created_at,
            attachments: Vec::new(),
            usage: None,
            breaks: None,
            messages: Vec::new(),
            posted_by: None,
        });
        let cache = IndexCache::default();
        TalkView::with_config(talk.clone(), false, None, home.path(), &cache);
        assert!(cache_lock(&cache.snapshot).is_none());
        assert!(cache_lock(&cache.reviews).is_none());
        talk.turns[0].body = "dead の修正を待つ".into();
        TalkView::with_config(talk.clone(), false, None, home.path(), &cache);
        let index = Arc::clone(&cache_lock(&cache.snapshot).as_ref().unwrap().ordinary);
        assert!(cache_lock(&cache.reviews).is_none());
        TalkView::with_config(talk.clone(), false, None, home.path(), &cache);
        assert!(Arc::ptr_eq(
            &index,
            &cache_lock(&cache.snapshot).as_ref().unwrap().ordinary
        ));
        talk.turns[0].body = "R2-1-3 の修正を待つ".into();
        TalkView::with_config(talk, false, None, home.path(), &cache);
        assert!(cache_lock(&cache.reviews).is_some());
    }
    #[test]
    fn references_validate_all_record_kinds_and_refresh_after_repairs_and_corruption() {
        let home = tempfile::tempdir().unwrap();
        let roots = [
            home.path().join("runs"),
            home.path().join("queue"),
            home.path().join("questions"),
            home.path().join("talks"),
        ];
        let queue = Queue::at(roots[1].clone());
        let questions = Questions::at(roots[2].clone());
        let talks = Talks::at(roots[3].clone());
        let cache = IndexCache::default();
        let mut run = RunState::new(
            home.path().into(),
            "main".into(),
            "base".into(),
            "instruction".into(),
            Config::default(),
        );
        run.id = "20260908-205802-6a06".into();
        let mut task = Task::new(
            "task".into(),
            "instruction".into(),
            home.path().into(),
            Source::Human,
        );
        task.id = "20260908-205803-12ba".into();
        let mut question = ask::Question::new(
            run.id.clone(),
            "merge".into(),
            "operator".into(),
            "Approve?".into(),
            "detail".into(),
            Vec::new(),
        );
        question.id = "20260908-205805-d678".into();
        question.status = ask::QuestionStatus::Answered;
        let talk: Talk = serde_json::from_value(serde_json::json!({
            "schema": talk::SCHEMA, "id": "20260908-205806-6f0d", "repo": ".", "agent": "test", "status": "open",
            "turns": [], "created_at": "2026-09-08T20:58:02Z", "updated_at": "2026-09-08T20:58:02Z",
            "seat": crate::agent::SeatState::new("chat", "test", 1)
        })).unwrap();
        let records = [
            (
                run.id.as_str(),
                roots[0].join(&run.id).join("run.json"),
                serde_json::to_vec(&run).unwrap(),
            ),
            (
                task.id.as_str(),
                queue.path_of(&task.id),
                serde_json::to_vec(&task).unwrap(),
            ),
            (
                question.id.as_str(),
                questions.path_of(&question.id),
                serde_json::to_vec(&question).unwrap(),
            ),
            (
                talk.id.as_str(),
                talks.root().join(format!("{}.json", talk.id)),
                serde_json::to_vec(&talk).unwrap(),
            ),
        ];
        for (i, (_, path, _)) in records.iter().enumerate() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, if i % 2 == 0 { "invalid JSON" } else { "{}" }).unwrap();
        }
        let get = || cache.get(&roots[0], &queue, &questions, &talks, false);
        let invalid = get();
        for (id, _, _) in &records {
            assert!(invalid.resolve(id, false).is_none(), "{id}");
            assert!(
                invalid.resolve(crate::queue::short(id), false).is_none(),
                "{id}"
            );
            let prose = format!("{} `{id}`", crate::queue::short(id));
            assert!(
                invalid
                    .parts(&prose, false)
                    .iter()
                    .all(|p| p.href.is_none())
            );
            let mut nodes = md::to_nodes(&prose, &md::ImageBase::None);
            invalid.markdown(&mut nodes);
            assert!(
                !serde_json::to_string(&nodes)
                    .unwrap()
                    .contains("internal_link")
            );
        }
        assert!(Arc::ptr_eq(&invalid, &get()));
        for (_, path, valid) in &records {
            std::fs::write(path, valid).unwrap();
        }
        let repaired = get();
        for (id, _, _) in &records {
            assert!(repaired.resolve(id, false).is_some(), "{id}");
            assert!(
                repaired.resolve(crate::queue::short(id), false).is_some(),
                "{id}"
            );
        }
        for (_, path, _) in &records {
            std::fs::write(path, "broken again").unwrap();
        }
        let corrupted = get();
        for (id, _, _) in &records {
            assert!(corrupted.resolve(id, false).is_none(), "{id}");
        }
    }
}
