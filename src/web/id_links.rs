//! Store-backed references shared by Markdown and plain-text UI surfaces.
use super::*;

#[derive(Default)]
pub(super) struct IdIndex {
    entries: Vec<Entry>,
}
struct Entry {
    kind: &'static str,
    id: String,
    href: String,
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

pub(super) fn page_href(route: &str, id: &str) -> String {
    format!("#/{route}/{}", encode_segment(id))
}

impl IdIndex {
    pub(super) fn load(runs: &FsPath, queue: &Queue, questions: &Questions, talks: &Talks) -> Self {
        let mut index = Self::default();
        for id in run_ids(runs) {
            if read_run(runs, &id).is_ok() {
                index.add("run", &id, page_href("runs", &id));
            }
        }
        for task in queue.list() {
            index.add("task", &task.id, page_href("tasks", &task.id));
        }
        for q in questions.list() {
            index.add("question", &q.id, page_href("questions", &q.id));
        }
        for t in talks.list() {
            index.add("chat", &t.id, page_href("chat", &t.id));
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
        });
    }

    fn resolve(&self, token: &str, code: bool) -> Option<String> {
        let (kind, id) = match token.split_once('@') {
            Some(("run", id)) => (Some("run"), id),
            Some(("task", id)) => (Some("task"), id),
            Some(("chat", id)) => (Some("chat"), id),
            Some(("question" | "approval" | "merge-approval", id)) => (Some("question"), id),
            Some(_) => return None,
            None => (None, token),
        };
        let short = id.len() == 4 && id.bytes().all(|b| b.is_ascii_hexdigit());
        if short && !code && kind.is_none() && !id.bytes().any(|b| b.is_ascii_digit()) {
            return None;
        }
        let eligible = |e: &&Entry| kind.is_none_or(|kind| e.kind == kind);
        let exact: Vec<_> = self
            .entries
            .iter()
            .filter(eligible)
            .filter(|e| e.id == id)
            .collect();
        let matches = if exact.is_empty() && short {
            self.entries
                .iter()
                .filter(eligible)
                .filter(|e| crate::queue::short(&e.id) == id)
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
) -> ApiResult<Json<Vec<Vec<Part>>>> {
    if texts.len() > 64 || texts.iter().map(|t| t.text.len()).sum::<usize>() > 65_536 {
        return Err(ApiError::bad_request("reference batch too large"));
    }
    blocking(move || {
        let mut index = IdIndex::load(&ui.runs, &ui.queue, &ui.questions, &ui.talks);
        if texts.iter().any(|t| t.text.contains('R')) {
            index.findings(&ui.runs);
        }
        Ok(Json(
            texts.iter().map(|t| index.parts(&t.text, t.code)).collect(),
        ))
    })
    .await
}

/// Check all visible message bodies, including legacy turns and queued notices.
pub(super) fn has_findings(talk: &Talk) -> bool {
    talk.turns
        .iter()
        .any(|t| t.body.contains('R') || t.messages.iter().any(|m| m.body.contains('R')))
        || talk.pending_messages.iter().any(|m| m.body.contains('R'))
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
        assert!(index.resolve("dead", false).is_none());
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
            "6a06 **12ba** `dead 6a06` [6a06](https://example.org)\n\n```\n6a06\n```",
            &md::ImageBase::None,
        );
        index().markdown(&mut nodes);
        let json = serde_json::to_value(&nodes).unwrap();
        let encoded = json.to_string();
        assert_eq!(encoded.matches("internal_link").count(), 4);
        assert!(encoded.contains("code_block"));
        assert!(encoded.contains("https://example.org"));
        assert!(encoded.contains("\"type\":\"code\""));
    }

    #[test]
    fn references_load_only_readable_existing_store_records() {
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
        std::fs::write(runs.join("20260908-205802-6a06/run.json"), "{}").unwrap();
        let index = IdIndex::load(
            &runs,
            &queue,
            &questions,
            &Talks::at(temp.path().join("talks")),
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
        let refreshed = IdIndex::load(
            &runs,
            &queue,
            &questions,
            &Talks::at(temp.path().join("talks")),
        );
        assert!(refreshed.resolve("12ba", false).is_none());
    }
}
