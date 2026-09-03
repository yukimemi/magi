//! GFM markdown, parsed into a serializable node tree instead of HTML.
//!
//! The web UI's one hard rule is that DOM is built with `createElement` and
//! `textContent` and never with `innerHTML` — an AI-authored chat reply, a
//! question's reasoning, and an operator's task instruction are all arbitrary
//! text, and any of them could contain a `<script>` tag. That rule used to
//! force the front end into `assets/ui/app.js`'s own tiny hand-rolled
//! markdown reader, which understood four constructs and left everything
//! else — tables, task lists, links — as literal asterisks and brackets.
//!
//! This module moves the actual parsing to [`comrak`], a full GFM
//! implementation, and hands the client a tree of [`Node`] instead of a
//! string of HTML. The client walks the tree and builds elements directly;
//! there is never a markup string to insert, so the no-`innerHTML` rule holds
//! even though the markdown support is now complete.
//!
//! Two things are deliberately stricter than the source markdown, both
//! because this tree can carry attacker-authored text (an AI writes chat
//! turns and question detail) into a browser with no server-side sanitizer
//! standing between them:
//!
//! - [`normalize_link`] keeps only `http:`/`https:` link destinations.
//! - [`normalize_image`] keeps only `data:` image URIs and, when the
//!   markdown came from a question, relative filenames resolved against that
//!   question's existing sandboxed panel asset route (see
//!   [`ImageBase::QuestionPanel`]).
//!
//! Raw HTML in the source (a `<script>` block, an `<img onerror=…>`) is never
//! interpreted: [`to_nodes`] turns it into a [`Node::Text`] carrying the
//! literal characters, so a client that renders it with `textContent` shows
//! the tag rather than running it.

use comrak::nodes::{AstNode, ListType, NodeValue, TableAlignment};
use comrak::{Arena, Options, parse_document};
use serde::Serialize;

use crate::ask::valid_asset_name;

/// How a relative image path in the source markdown is allowed to resolve.
///
/// A relative path (`![shot](shot.png)`) names no host and no scheme, so on
/// its own it is not renderable at all — magi's web UI never serves files
/// from an arbitrary directory. The one place a relative path is meaningful
/// is a question's `detail`, where it names one of that question's own panel
/// assets, already reachable at `GET /api/questions/{id}/panel/{name}`. Every
/// other caller passes [`ImageBase::None`], under which a relative path is
/// rejected exactly like a `file:` URL.
#[derive(Debug, Clone)]
pub enum ImageBase {
    /// No question backs this text, so a relative image path cannot be
    /// resolved.
    None,
    /// This text is a question's `detail`; a relative image path resolves
    /// against that question's panel asset route.
    QuestionPanel {
        /// The question id the panel route is scoped to.
        id: String,
    },
}

/// Column alignment of a table cell, per the GFM table extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    /// No alignment was requested for this column.
    None,
    /// `:---`
    Left,
    /// `:---:`
    Center,
    /// `---:`
    Right,
}

fn align_of(a: TableAlignment) -> Align {
    match a {
        TableAlignment::None => Align::None,
        TableAlignment::Left => Align::Left,
        TableAlignment::Center => Align::Center,
        TableAlignment::Right => Align::Right,
    }
}

/// One cell of a [`Node::Table`] row.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TableCell {
    /// Is this cell part of the header row?
    pub header: bool,
    /// This cell's column alignment.
    pub align: Align,
    /// The cell's inline content.
    pub children: Vec<Node>,
}

/// One node of parsed markdown.
///
/// Shaped for a client to walk and turn into DOM nodes directly — every
/// variant is either a container (`children`, or for a list or table, a
/// nested collection) or a leaf that carries exactly the data needed to
/// build one element. There is no HTML anywhere in this type; see the module
/// documentation for the two places that turn attacker-controlled markup
/// into plain [`Node::Text`] instead of a live link or image.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Node {
    /// A paragraph.
    Paragraph {
        /// Inline content.
        children: Vec<Node>,
    },
    /// A heading.
    Heading {
        /// Heading level, 1 through 6.
        level: u8,
        /// Inline content.
        children: Vec<Node>,
    },
    /// An unordered (bullet) list.
    BulletList {
        /// The list's items. Always [`Node::ListItem`].
        items: Vec<Node>,
    },
    /// An ordered (numbered) list.
    OrderedList {
        /// The number the list starts counting from.
        start: u32,
        /// The list's items. Always [`Node::ListItem`].
        items: Vec<Node>,
    },
    /// One item of a list, or of a GFM task list.
    ListItem {
        /// `Some(true)` for a checked task item, `Some(false)` for an
        /// unchecked one, `None` for a plain list item.
        checked: Option<bool>,
        /// The item's block content, which may include a nested list.
        children: Vec<Node>,
    },
    /// A GFM table.
    Table {
        /// One alignment per column, in column order.
        align: Vec<Align>,
        /// Rows in reading order; the first is the header row.
        rows: Vec<Vec<TableCell>>,
    },
    /// A block quote.
    BlockQuote {
        /// The quote's block content.
        children: Vec<Node>,
    },
    /// A horizontal rule (`---`).
    ThematicBreak,
    /// A fenced or indented code block.
    ///
    /// Never syntax-highlighted: magi does not add a highlighting
    /// dependency, so `lang` is carried only as a label for the client to
    /// show, not as a hint the server has already acted on.
    CodeBlock {
        /// The fence's info string (e.g. `rust`), if the source gave one.
        lang: Option<String>,
        /// The block's literal text.
        code: String,
    },
    /// An inline code span.
    Code {
        /// The span's literal text.
        code: String,
    },
    /// Emphasized (`*italic*`) inline content.
    Emphasis {
        /// Inline content.
        children: Vec<Node>,
    },
    /// Strongly emphasized (`**bold**`) inline content.
    Strong {
        /// Inline content.
        children: Vec<Node>,
    },
    /// Struck-through (`~~text~~`) inline content.
    Strikethrough {
        /// Inline content.
        children: Vec<Node>,
    },
    /// A link. Only ever produced for an `http:`/`https:` destination; any
    /// other scheme becomes [`Node::Text`] instead, see [`normalize_link`].
    Link {
        /// The link's destination.
        href: String,
        /// The link's inline text.
        children: Vec<Node>,
    },
    /// An image. Only ever produced for a `data:` image URI or a resolved
    /// question panel asset; anything else becomes [`Node::Text`] instead,
    /// see [`normalize_image`].
    Image {
        /// The image's resolved source.
        src: String,
        /// The image's alt text.
        alt: String,
    },
    /// A soft line break: a single newline in the source, conventionally
    /// rendered as a space or an ordinary wrap.
    SoftBreak,
    /// A hard line break: two trailing spaces, or a trailing backslash, in
    /// the source.
    LineBreak,
    /// Plain text.
    ///
    /// Also stands in for anything [`to_nodes`] refuses to render as markup:
    /// raw HTML from the source, a link with a disallowed scheme, an image
    /// with a disallowed source.
    Text {
        /// The text itself.
        value: String,
    },
}

/// Parse `text` as GFM markdown into a node tree.
///
/// Enables the GFM extensions an operator or an agent's prose actually uses —
/// tables, task lists, strikethrough, autolinked bare URLs — and nothing
/// that changes how plain prose reads (no smart quotes, no superscript).
/// `image_base` controls whether a relative image path in `text` can resolve
/// to anything; see [`ImageBase`].
pub fn to_nodes(text: &str, image_base: &ImageBase) -> Vec<Node> {
    let arena = Arena::new();
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    let root = parse_document(&arena, text, &options);
    children_of(root, image_base)
}

fn children_of<'a>(node: &'a AstNode<'a>, image_base: &ImageBase) -> Vec<Node> {
    node.children()
        .filter_map(|child| convert(child, image_base))
        .collect()
}

fn convert<'a>(node: &'a AstNode<'a>, image_base: &ImageBase) -> Option<Node> {
    let value = node.data.borrow().value.clone();
    Some(match value {
        NodeValue::Paragraph => Node::Paragraph {
            children: children_of(node, image_base),
        },
        NodeValue::Heading(h) => Node::Heading {
            level: h.level,
            children: children_of(node, image_base),
        },
        NodeValue::List(l) => {
            let items = children_of(node, image_base);
            if l.list_type == ListType::Ordered {
                Node::OrderedList {
                    start: l.start as u32,
                    items,
                }
            } else {
                Node::BulletList { items }
            }
        }
        NodeValue::Item(_) => Node::ListItem {
            checked: None,
            children: children_of(node, image_base),
        },
        NodeValue::TaskItem(t) => Node::ListItem {
            checked: Some(t.symbol.is_some()),
            children: children_of(node, image_base),
        },
        NodeValue::BlockQuote => Node::BlockQuote {
            children: children_of(node, image_base),
        },
        NodeValue::ThematicBreak => Node::ThematicBreak,
        NodeValue::CodeBlock(cb) => Node::CodeBlock {
            lang: (!cb.info.is_empty()).then_some(cb.info),
            code: cb.literal,
        },
        NodeValue::Code(c) => Node::Code { code: c.literal },
        // Raw HTML is never interpreted: the literal source text becomes a
        // text node, so a client rendering it with `textContent` shows the
        // tag's characters instead of running or styling anything.
        NodeValue::HtmlBlock(h) => Node::Text { value: h.literal },
        NodeValue::HtmlInline(s) => Node::Text { value: s },
        NodeValue::Text(s) => Node::Text {
            value: s.into_owned(),
        },
        NodeValue::SoftBreak => Node::SoftBreak,
        NodeValue::LineBreak => Node::LineBreak,
        NodeValue::Emph => Node::Emphasis {
            children: children_of(node, image_base),
        },
        NodeValue::Strong => Node::Strong {
            children: children_of(node, image_base),
        },
        NodeValue::Strikethrough => Node::Strikethrough {
            children: children_of(node, image_base),
        },
        NodeValue::Link(l) => normalize_link(&l.url, children_of(node, image_base)),
        NodeValue::Image(l) => {
            let alt = plain_text(&children_of(node, image_base));
            normalize_image(&l.url, alt, image_base)
        }
        NodeValue::Table(t) => {
            let rows = node
                .children()
                .map(|row| table_row(row, &t.alignments, image_base))
                .collect();
            Node::Table {
                align: t.alignments.iter().copied().map(align_of).collect(),
                rows,
            }
        }
        // Everything else is either unreachable (`TableRow`/`TableCell`,
        // handled inside `table_row` rather than through this generic walk)
        // or an extension `to_nodes` never turns on (footnotes, math,
        // wikilinks, alerts, ...), so it cannot appear in a tree this module
        // produced.
        _ => return None,
    })
}

fn table_row<'a>(
    row: &'a AstNode<'a>,
    aligns: &[TableAlignment],
    image_base: &ImageBase,
) -> Vec<TableCell> {
    let header = matches!(row.data.borrow().value, NodeValue::TableRow(true));
    row.children()
        .enumerate()
        .map(|(i, cell)| TableCell {
            header,
            align: aligns.get(i).copied().map(align_of).unwrap_or(Align::None),
            children: children_of(cell, image_base),
        })
        .collect()
}

/// The plain-text reading of a node tree: every [`Node::Text`]/[`Node::Code`]
/// leaf's characters, recursively, with breaks turned into whitespace.
///
/// Used for an image's alt text (comrak keeps it as the image's inline
/// children rather than a separate string) and for the label of a link whose
/// scheme [`normalize_link`] refuses to keep live.
fn plain_text(nodes: &[Node]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            Node::Text { value } | Node::Code { code: value } => out.push_str(value),
            Node::Image { alt, .. } => out.push_str(alt),
            Node::SoftBreak => out.push(' '),
            Node::LineBreak => out.push('\n'),
            Node::Paragraph { children }
            | Node::Heading { children, .. }
            | Node::Emphasis { children }
            | Node::Strong { children }
            | Node::Strikethrough { children }
            | Node::BlockQuote { children }
            | Node::ListItem { children, .. }
            | Node::Link { children, .. } => out.push_str(&plain_text(children)),
            Node::BulletList { .. }
            | Node::OrderedList { .. }
            | Node::Table { .. }
            | Node::ThematicBreak
            | Node::CodeBlock { .. } => {}
        }
    }
    out
}

/// Keep a link live only for `http:`/`https:`; everything else — a
/// `javascript:`/`data:`/`vbscript:`/`file:` scheme, or no scheme at all —
/// is not a destination magi's web UI will navigate to, so the link
/// disappears and its label survives as plain text.
fn normalize_link(url: &str, children: Vec<Node>) -> Node {
    let allowed = match url.split_once(':') {
        Some((scheme, _)) => {
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
        }
        None => false,
    };
    if allowed {
        Node::Link {
            href: url.to_owned(),
            children,
        }
    } else {
        Node::Text {
            value: plain_text(&children),
        }
    }
}

/// Keep an image live only for a `data:` image URI, or — when `image_base`
/// names a question — a bare relative filename resolved against that
/// question's own panel asset route. An absolute `https://` image, a
/// protocol-relative `//` image, and a relative path outside a question's
/// panel all fail both checks and fall back to alt text plus the URL, so the
/// operator still sees what the agent meant to show without magi's UI
/// fetching anything from outside itself.
fn normalize_image(url: &str, alt: String, image_base: &ImageBase) -> Node {
    if url.to_ascii_lowercase().starts_with("data:image/") {
        return Node::Image {
            src: url.to_owned(),
            alt,
        };
    }
    if let ImageBase::QuestionPanel { id } = image_base {
        if valid_asset_name(url) {
            return Node::Image {
                src: format!("/api/questions/{id}/panel/{url}"),
                alt,
            };
        }
    }
    let value = if alt.is_empty() {
        url.to_owned()
    } else {
        format!("{alt} ({url})")
    };
    Node::Text { value }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn nodes(text: &str) -> Vec<Node> {
        to_nodes(text, &ImageBase::None)
    }

    fn text(s: &str) -> Node {
        Node::Text {
            value: s.to_owned(),
        }
    }

    #[test]
    fn a_heading_carries_its_level() {
        assert_eq!(
            nodes("### Three"),
            vec![Node::Heading {
                level: 3,
                children: vec![text("Three")],
            }]
        );
    }

    #[test]
    fn emphasis_and_strong_and_strikethrough_each_get_their_own_node() {
        assert_eq!(
            nodes("*i* **b** ~~s~~"),
            vec![Node::Paragraph {
                children: vec![
                    Node::Emphasis {
                        children: vec![text("i")]
                    },
                    text(" "),
                    Node::Strong {
                        children: vec![text("b")]
                    },
                    text(" "),
                    Node::Strikethrough {
                        children: vec![text("s")]
                    },
                ],
            }]
        );
    }

    #[test]
    fn a_bullet_list_is_a_bullet_list() {
        assert_eq!(
            nodes("- one\n- two"),
            vec![Node::BulletList {
                items: vec![
                    Node::ListItem {
                        checked: None,
                        children: vec![Node::Paragraph {
                            children: vec![text("one")]
                        }],
                    },
                    Node::ListItem {
                        checked: None,
                        children: vec![Node::Paragraph {
                            children: vec![text("two")]
                        }],
                    },
                ],
            }]
        );
    }

    #[test]
    fn an_ordered_list_keeps_its_start_number() {
        let Some(Node::OrderedList { start, items }) = nodes("5. five\n6. six").into_iter().next()
        else {
            panic!("expected an ordered list");
        };
        assert_eq!(start, 5);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn a_nested_list_is_a_list_item_containing_a_list() {
        let doc = nodes("- outer\n  - inner");
        let Some(Node::BulletList { items }) = doc.into_iter().next() else {
            panic!("expected a bullet list");
        };
        let Node::ListItem { children, .. } = &items[0] else {
            panic!("expected a list item");
        };
        assert!(
            children
                .iter()
                .any(|c| matches!(c, Node::BulletList { .. })),
            "the outer item's children should hold the nested list: {children:?}"
        );
    }

    #[test]
    fn task_list_items_carry_their_checked_state() {
        let Some(Node::BulletList { items }) = nodes("- [ ] todo\n- [x] done").into_iter().next()
        else {
            panic!("expected a bullet list");
        };
        assert_eq!(items.len(), 2);
        assert!(matches!(
            items[0],
            Node::ListItem {
                checked: Some(false),
                ..
            }
        ));
        assert!(matches!(
            items[1],
            Node::ListItem {
                checked: Some(true),
                ..
            }
        ));
    }

    #[test]
    fn a_table_keeps_its_header_and_its_column_alignment() {
        let md = "| a | b |\n|:--|--:|\n| 1 | 2 |\n";
        let Some(Node::Table { align, rows }) = nodes(md).into_iter().next() else {
            panic!("expected a table");
        };
        assert_eq!(align, vec![Align::Left, Align::Right]);
        assert_eq!(rows.len(), 2, "a header row and one body row: {rows:?}");
        assert!(rows[0][0].header, "the first row is the header: {rows:?}");
        assert!(!rows[1][0].header, "the body row is not a header: {rows:?}");
        assert_eq!(rows[0][0].align, Align::Left);
        assert_eq!(rows[0][1].align, Align::Right);
    }

    #[test]
    fn a_block_quote_is_a_block_quote() {
        assert_eq!(
            nodes("> quoted"),
            vec![Node::BlockQuote {
                children: vec![Node::Paragraph {
                    children: vec![text("quoted")]
                }],
            }]
        );
    }

    #[test]
    fn a_thematic_break_needs_nothing_else() {
        assert_eq!(nodes("---"), vec![Node::ThematicBreak]);
    }

    #[test]
    fn inline_code_is_never_interpreted_as_markdown() {
        assert_eq!(
            nodes("`*not italic*`"),
            vec![Node::Paragraph {
                children: vec![Node::Code {
                    code: "*not italic*".to_owned()
                }],
            }]
        );
    }

    #[test]
    fn a_fenced_code_block_carries_its_language_but_no_color() {
        assert_eq!(
            nodes("```rust\nfn x() {}\n```"),
            vec![Node::CodeBlock {
                lang: Some("rust".to_owned()),
                code: "fn x() {}\n".to_owned(),
            }]
        );
    }

    #[test]
    fn an_http_link_stays_a_link() {
        assert_eq!(
            nodes("[go](https://example.com/x)"),
            vec![Node::Paragraph {
                children: vec![Node::Link {
                    href: "https://example.com/x".to_owned(),
                    children: vec![text("go")],
                }],
            }]
        );
    }

    #[test]
    fn a_javascript_link_is_not_a_link_node_at_all() {
        let doc = nodes("[x](javascript:alert(1))");
        // No node anywhere in the tree may be `Node::Link`.
        fn has_link(nodes: &[Node]) -> bool {
            nodes.iter().any(|n| match n {
                Node::Link { .. } => true,
                Node::Paragraph { children }
                | Node::Heading { children, .. }
                | Node::Emphasis { children }
                | Node::Strong { children }
                | Node::Strikethrough { children }
                | Node::BlockQuote { children }
                | Node::ListItem { children, .. } => has_link(children),
                _ => false,
            })
        }
        assert!(!has_link(&doc), "must not contain a link node: {doc:?}");
        assert_eq!(
            doc,
            vec![Node::Paragraph {
                children: vec![text("x")]
            }]
        );
    }

    #[test]
    fn an_absolute_https_image_does_not_render() {
        let doc = nodes("![a](https://example.com/x.png)");
        assert_eq!(
            doc,
            vec![Node::Paragraph {
                children: vec![text("a (https://example.com/x.png)")]
            }]
        );
    }

    #[test]
    fn a_data_uri_image_renders() {
        let doc = nodes("![a](data:image/png;base64,AAAA)");
        assert_eq!(
            doc,
            vec![Node::Paragraph {
                children: vec![Node::Image {
                    src: "data:image/png;base64,AAAA".to_owned(),
                    alt: "a".to_owned(),
                }],
            }]
        );
    }

    #[test]
    fn a_question_relative_image_resolves_to_its_panel_route() {
        let base = ImageBase::QuestionPanel {
            id: "20260903-014455-ab12".to_owned(),
        };
        let doc = to_nodes("![shot](shot.png)", &base);
        assert_eq!(
            doc,
            vec![Node::Paragraph {
                children: vec![Node::Image {
                    src: "/api/questions/20260903-014455-ab12/panel/shot.png".to_owned(),
                    alt: "shot".to_owned(),
                }],
            }]
        );
    }

    #[test]
    fn a_protocol_relative_image_does_not_render_even_with_a_question_base() {
        let base = ImageBase::QuestionPanel {
            id: "20260903-014455-ab12".to_owned(),
        };
        let doc = to_nodes("![a](//evil.example/x.png)", &base);
        assert!(
            !doc.iter().any(|n| matches!(n, Node::Paragraph { children } if children.iter().any(|c| matches!(c, Node::Image { .. })))),
            "a protocol-relative source must never become an image: {doc:?}"
        );
    }

    #[test]
    fn raw_html_becomes_text_everywhere_in_the_tree() {
        let doc = nodes("before <script>alert(1)</script> after");
        fn contains_html_markup(nodes: &[Node]) -> bool {
            nodes.iter().any(|n| match n {
                Node::Text { value } => value.contains("<script"),
                Node::Paragraph { children }
                | Node::Heading { children, .. }
                | Node::Emphasis { children }
                | Node::Strong { children }
                | Node::Strikethrough { children }
                | Node::BlockQuote { children }
                | Node::ListItem { children, .. } => contains_html_markup(children),
                _ => false,
            })
        }
        assert!(
            contains_html_markup(&doc),
            "the literal tag text must survive as a text node: {doc:?}"
        );
        // And, symmetrically: no node in the tree may claim to *be* HTML —
        // there is no such variant, so this is really asserting the parse
        // produced ordinary text/paragraph nodes and nothing else.
        for node in &doc {
            assert!(
                matches!(node, Node::Paragraph { .. }),
                "a document with only text and an HTML span is one paragraph: {doc:?}"
            );
        }
    }

    #[test]
    fn a_block_level_script_tag_becomes_a_text_node_too() {
        let doc = nodes("<script>alert(1)</script>");
        assert_eq!(
            doc,
            vec![text("<script>alert(1)</script>")],
            "an HTML block is one literal text node, not markup: {doc:?}"
        );
    }
}
