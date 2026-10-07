//! Posting gate shared by GitHub titles, descriptions and comments.
use crate::run::RunState;
use crate::scrub::{Identity, scrub};

/// Fixed fallback for a rejected title.
pub const NEUTRAL_TITLE: &str = "chore: update repository";
/// Fixed fallback for a rejected description or comment.
pub const NEUTRAL_BODY: &str = "Generated GitHub text was withheld by the magi posting gate. Review the branch diff and the local run report for details.";

/// Categories only: diagnostics must never repeat the offending secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// Title contains a meaningful share of non-English letters.
    TitleLanguage,
    /// Unquoted body prose contains a meaningful share of non-English letters.
    BodyLanguage,
    /// Shared redaction rules found sensitive or local data.
    SensitiveData,
}

/// Pure checker. Language exemptions never exempt sensitive data.
pub fn check(title: &str, body: &str) -> Vec<Violation> {
    let mut out = Vec::new();
    if non_english(title) {
        out.push(Violation::TitleLanguage);
    }
    if non_english(&prose(body)) {
        out.push(Violation::BodyLanguage);
    }
    let id = Identity::default();
    if scrub(title, &id) != title || scrub(body, &id) != body {
        out.push(Violation::SensitiveData);
    }
    out
}

/// Function words and common verbs of the Latin-script languages most likely
/// to appear, chosen to avoid ordinary English words.
const FOREIGN_WORDS: &[&str] = &[
    "este",
    "esta",
    "para",
    "los",
    "las",
    "del",
    "una",
    "que",
    "por",
    "errores",
    "corregir",
    "agrega",
    "cambio",
    "solicitudes",
    "fallidas",
    "reintentos",
    "les",
    "des",
    "pour",
    "avec",
    "dans",
    "est",
    "une",
    "pas",
    "und",
    "der",
    "das",
    "nicht",
    "mit",
    "ein",
    "eine",
    "für",
    "wird",
    "não",
    "uma",
    "della",
    "che",
    "con",
    "fehler",
    "corrigir",
    "erreurs",
    "bitte",
    "anfrage",
    "anfragen",
    "wiederholen",
    "korrigieren",
];

/// English function words and everyday development vocabulary. Text whose
/// words never meet this list is not English, whatever FOREIGN_WORDS knows.
/// Words spelled the same in German or Dutch (will, die, also) stay out.
const ENGLISH_WORDS: &[&str] = &[
    "the",
    "a",
    "an",
    "to",
    "of",
    "and",
    "or",
    "for",
    "in",
    "on",
    "at",
    "by",
    "with",
    "from",
    "when",
    "while",
    "this",
    "that",
    "these",
    "those",
    "is",
    "are",
    "be",
    "was",
    "were",
    "been",
    "not",
    "no",
    "it",
    "its",
    "as",
    "if",
    "so",
    "but",
    "than",
    "then",
    "into",
    "after",
    "before",
    "only",
    "can",
    "should",
    "must",
    "may",
    "has",
    "have",
    "had",
    "do",
    "does",
    "all",
    "any",
    "each",
    "one",
    "two",
    "new",
    "old",
    "more",
    "less",
    "which",
    "what",
    "why",
    "how",
    "now",
    "never",
    "always",
    "instead",
    "without",
    "within",
    "over",
    "under",
    "per",
    "via",
    "we",
    "you",
    "they",
    "there",
    "their",
    "your",
    "our",
    "use",
    "used",
    "uses",
    "make",
    "makes",
    "fix",
    "add",
    "update",
    "remove",
    "bump",
    "refactor",
    "test",
    "retry",
    "request",
    "review",
    "run",
    "task",
    "branch",
    "merge",
    "release",
    "error",
    "config",
    "build",
    "check",
    "docs",
    "feat",
    "chore",
    "change",
    "changes",
    "file",
    "code",
    "repository",
    "repo",
    "pull",
    "commit",
    "message",
    "text",
    "title",
    "body",
    "github",
    "gate",
    "default",
    "value",
    "key",
    "name",
    "path",
    "state",
    "agent",
    "seat",
    "fail",
    "failure",
    "failed",
    "pass",
    "read",
    "write",
    "set",
    "get",
    "send",
    "post",
    "open",
    "close",
    "start",
    "stop",
    "keep",
    "drop",
    "move",
    "rename",
    "handle",
    "support",
    "allow",
    "avoid",
    "ensure",
    "prevent",
    "background",
    "summary",
    "risk",
    "risks",
    "follow",
    "verify",
    "hand",
    "version",
    "bug",
    "issue",
    "finding",
    "findings",
    "round",
    "rounds",
    "queue",
    "worktree",
    "diff",
    "line",
    "lines",
    "word",
    "words",
    "list",
    "case",
    "cases",
    "input",
    "output",
    "result",
    "results",
    "fallback",
    "replace",
    "replaced",
    "withheld",
    "generated",
    "neutral",
    "english",
    "language",
    "detect",
    "detection",
    "match",
    "matches",
    "wrong",
    "stale",
    "missing",
    "extra",
    "small",
    "let",
    "settle",
    "carry",
    "note",
    "help",
    "colour",
    "color",
    "palette",
    "deputy",
    "brief",
    "briefs",
    "serde",
    "tokio",
];

fn english_words(text: &str) -> (usize, usize) {
    // A conventional-commit prefix (`fix(scope)!:`) says nothing about the
    // language; drop it from the raw text, before punctuation is flattened.
    let t = text.trim_start();
    let kind = t.chars().take_while(|c| c.is_ascii_lowercase()).count();
    let mut rest = &t[kind..];
    if kind > 0 && rest.starts_with('(') {
        rest = rest.find(')').map_or(rest, |i| &rest[i + 1..]);
    }
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    let text = if kind > 0 && rest.starts_with(':') {
        &rest[1..]
    } else {
        text
    };
    let cleaned: String = text
        .chars()
        .map(|c| {
            if "()[],;!?\"'*#<>`-=|".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let (mut counted, mut hits) = (0, 0);
    let tokens = cleaned.split_whitespace();
    for raw in tokens {
        let w = raw.trim_matches(|c| c == ':' || c == '.');
        if w.len() < 2 || !w.chars().all(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        if w.chars().all(|c| c.is_ascii_uppercase())
            || w.chars().skip(1).any(|c| c.is_ascii_uppercase())
        {
            continue;
        }
        counted += 1;
        let w = w.to_ascii_lowercase();
        let known = |x: &str| ENGLISH_WORDS.contains(&x);
        let stem = |suffix: &str, add: &str| w.strip_suffix(suffix).map(|b| format!("{b}{add}"));
        if known(&w)
            || [
                stem("ies", "y"),
                stem("es", ""),
                stem("s", ""),
                stem("ed", ""),
                stem("ed", "e"),
                stem("ing", ""),
                stem("ing", "e"),
            ]
            .iter()
            .flatten()
            .any(|x| known(x))
        {
            hits += 1;
        }
    }
    (counted, hits)
}

/// Word endings that are common in German, Dutch, Spanish and Italian and
/// rare in English. Only ever applied to long ASCII words (see `foreign_looking`).
const FOREIGN_SUFFIXES: &[&str] = &[
    "ieren", "ierung", "ungen", "ung", "keit", "heit", "lich", "zeit", "zeiten", "mente", "zione",
];

/// Prose shorter than this many counted words cannot be judged by a zero-hit
/// result alone: a title like `Improve performance` has no word the list knows.
const PROSE_WORDS: usize = 5;

/// Positive evidence that a short text is not English, without needing a
/// match against the English list: non-ASCII letters, any known foreign word,
/// or a long ASCII word with a foreign ending (`Wartezeiten`, `reduzieren`).
fn foreign_looking(text: &str) -> bool {
    text.chars().any(|c| c.is_alphabetic() && !c.is_ascii())
        || text
            .split(|c: char| !c.is_alphabetic())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .any(|w| {
                FOREIGN_WORDS.contains(&w.as_str())
                    || (w.len() >= 6
                        && w.is_ascii()
                        && FOREIGN_SUFFIXES.iter().any(|s| w.ends_with(s)))
            })
}

fn lacks_english(text: &str) -> bool {
    let (counted, hits) = english_words(text);
    if counted < PROSE_WORDS {
        // Too short for "no English word" to mean anything by itself.
        return (hits == 0 || hits * 2 < counted) && foreign_looking(text);
    }
    hits == 0 || hits * 3 < counted
}

fn foreign_words(text: &str) -> bool {
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let hits = words
        .iter()
        .filter(|w| FOREIGN_WORDS.contains(&w.as_str()))
        .count();
    hits >= 2 && hits * 4 >= words.len()
}

fn non_english(text: &str) -> bool {
    if foreign_words(text)
        || lacks_english(text)
        || text
            .split("\n\n")
            .any(|p| p.split_whitespace().count() >= 5 && lacks_english(p))
    {
        return true;
    }
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    let foreign = text
        .chars()
        .filter(|c| c.is_alphabetic() && !c.is_ascii())
        .count();
    // Accents and isolated identifiers are common in otherwise English prose.
    (foreign >= 4 && foreign * 10 >= letters.max(1))
        || text.lines().any(|line| {
            let letters = line.chars().filter(|c| c.is_alphabetic()).count();
            let foreign = line
                .chars()
                .filter(|c| c.is_alphabetic() && !c.is_ascii())
                .count();
            foreign >= 4 && foreign * 2 >= letters.max(1)
        })
}

/// Offset just past the first backtick run of exactly `n` in `text`, if any.
/// An unmatched opener is literal text in Markdown, so it exempts nothing.
fn closing_run(text: &str, n: usize) -> Option<usize> {
    let mut at = 0;
    while let Some(i) = text[at..].find('`') {
        let start = at + i;
        let len = text[start..].chars().take_while(|c| *c == '`').count();
        if len == n {
            return Some(start + len);
        }
        at = start + len;
    }
    None
}

fn prose(body: &str) -> String {
    let mut out = String::new();
    let mut details = 0usize;
    let mut quote = false;
    let mut fence: Option<(char, usize)> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some((marker, width)) = fence {
            if trimmed.chars().take_while(|c| *c == marker).count() >= width {
                fence = None;
            }
            continue;
        }
        let marker = trimmed.chars().next().unwrap_or(' ');
        let width = trimmed.chars().take_while(|c| *c == marker).count();
        if matches!(marker, '`' | '~') && width >= 3 {
            fence = Some((marker, width));
            continue;
        }
        if trimmed.starts_with('>') || line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        let mut rest = line;
        while !rest.is_empty() {
            if rest.starts_with('<')
                && let Some(end) = rest.find('>')
            {
                let tag = rest[..=end].to_ascii_lowercase();
                if tag.starts_with("<details") {
                    details += 1;
                } else if tag == "</details>" {
                    details = details.saturating_sub(1);
                } else if tag.starts_with("<blockquote") {
                    quote = true;
                } else if tag == "</blockquote>" {
                    quote = false;
                }
                rest = &rest[end + 1..];
                continue;
            }
            if rest.starts_with('`') {
                let n = rest.chars().take_while(|c| *c == '`').count();
                rest = match closing_run(&rest[n..], n) {
                    Some(end) => &rest[n + end..],
                    None => &rest[n..],
                };
                continue;
            }
            let c = rest.chars().next().expect("nonempty");
            if details == 0 && !quote {
                out.push(c);
            }
            rest = &rest[c.len_utf8()..];
        }
        out.push('\n');
    }
    out
}

/// Scrub local identity and pattern matches, then replace failing prose.
/// Every intervention is recorded without copying the offending material.
pub fn prepare(state: &mut RunState, title: &str, body: &str) -> (String, String) {
    let id = Identity::current();
    let clean_title = scrub(title, &id);
    let clean_body = scrub(body, &id);
    let violations = check(title, body);
    if clean_title != title || clean_body != body {
        state.event("github-text", "sensitive data removed before posting");
    }
    let language = state.config.graph.github_text_guard;
    let title = if language && violations.contains(&Violation::TitleLanguage) {
        state.event("github-text", "title replaced with neutral English text");
        NEUTRAL_TITLE.to_owned()
    } else {
        clean_title
    };
    let body = if language && violations.contains(&Violation::BodyLanguage) {
        state.event("github-text", "body replaced with neutral English text");
        NEUTRAL_BODY.to_owned()
    } else {
        clean_body
    };
    (title, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_text_language_and_exemptions() {
        assert_eq!(
            check("fix: retries", "日本語で変更の説明を書きます。"),
            vec![Violation::BodyLanguage]
        );
        assert!(check("fix: retries", "Add retries for failed requests.\n<details>\n<summary>Original task</summary>\n日本語の元の依頼です。\n</details>").is_empty());
        for body in [
            "Fix `cache\n\n日本語の説明を書きます。",
            "Fix `cache 日本語の説明を書きます。",
        ] {
            assert_eq!(
                check("fix: retries", body),
                vec![Violation::BodyLanguage],
                "{body}"
            );
        }
        for body in [
            "Add retries. `日本語の識別子`",
            "Add retries.\n```text\n日本語のコードです\n```",
            "Add retries.\n> 日本語の引用です",
            "Add retries.\n<blockquote>日本語の引用です</blockquote>",
            "Add retries. ``日本語 ` の識別子``",
            "Update café names.",
            "Change src/graph.rs and tests/common/mod.rs.",
        ] {
            assert!(check("fix: retries", body).is_empty(), "{body}");
        }
        for title in [
            "chore: release v0.95.0",
            "feat(deputy): let a settle carry a note",
            "feat(cli): colour --help in an Evangelion palette",
            "Refactor deputy briefs",
            "Bump tokio and serde",
        ] {
            assert!(check(title, "").is_empty(), "{title}");
        }
        assert!(check("Bitte Anfragen wiederholen", "").contains(&Violation::TitleLanguage));
        assert!(
            check("fix: retries", "Bitte Anfragen wiederholen").contains(&Violation::BodyLanguage)
        );
        assert!(
            check(
                "fix: retries",
                "Add retries for failed requests.\n\nBitte Anfragen schnell wiederholen heute."
            )
            .contains(&Violation::BodyLanguage)
        );
        assert!(
            check("fix: retries", "Zeitweise Sperren lösen Wartezeiten aus")
                .contains(&Violation::BodyLanguage)
        );
    }

    #[test]
    fn short_english_without_list_hits_passes_but_foreign_short_text_fails() {
        for text in [
            "Improve performance",
            "perf: speed cache",
            "Trim idle sockets",
            "Optimize memory consumption",
            "ci: pin actions",
            "Quicker warmup sprocket",
        ] {
            assert_eq!(english_words(text).1, 0, "{text} must have no list hit");
            assert!(check(text, "").is_empty(), "{text}");
            assert!(check("fix: retries", text).is_empty(), "{text}");
        }
        for text in [
            "fix(request): Wartezeiten reduzieren",
            "Leistung verbessern",
            "Corrección rápida",
            "fix: Wartezeiten im request reduzieren",
            "fix: retries schneller wiederholen",
            "fix(request): Wartezeiten bei retries reduzieren",
        ] {
            assert!(
                check(text, "").contains(&Violation::TitleLanguage),
                "{text}"
            );
            assert!(
                check("fix: retries", text).contains(&Violation::BodyLanguage),
                "{text}"
            );
        }
        // Four zero-hit words are short; five are prose and need a hit.
        let four = "Quicker warmup sprocket tweak";
        let five = "Quicker warmup sprocket tweak gizmo";
        assert_eq!(english_words(four), (4, 0));
        assert_eq!(english_words(five), (5, 0));
        assert!(check(four, "").is_empty());
        assert!(check(five, "").contains(&Violation::TitleLanguage));
    }

    #[test]
    fn github_text_sensitive_data_in_all_sections() {
        for secret in [
            "/Users/example/repo",
            "/home/example/repo",
            "C:\\Users\\Example\\repo",
            "/private/tmp/work",
            "dev@example.test",
            "ghp_abcdefghijklmnopqrstuv",
            "github_pat_abcdefghijklmnopqrstuv",
            "sk-abcdefghijklmnopqrstuv",
            "AKIAABCDEFGHIJKLMNOP",
            "password=example",
            "password=\"example\"",
            "token aBcdEfgHijkLmn0123456789",
            "buildbox.local",
            "token=abcdefghijklmnop012345",
            "Authorization: Bearer abcdefghijklmnop",
            "hostname=buildbox",
            "username=example",
            "10.2.3.4",
            "fe80::1",
        ] {
            assert!(
                check(secret, "").contains(&Violation::SensitiveData),
                "{secret}"
            );
            assert!(
                check(
                    "fix: retries",
                    &format!("<details>\n`{secret}`\n</details>")
                )
                .contains(&Violation::SensitiveData),
                "{secret}"
            );
        }
        for clean in [
            "https://github.com/example/repo",
            "src/graph.rs",
            "docs/home/example",
            "/api/v1/runs",
            "v1.2.3",
            "std::io::Error",
            "Use the token from the environment.",
        ] {
            assert!(check("fix: retries", clean).is_empty(), "{clean}");
        }
    }

    #[test]
    fn github_text_config_only_disables_language_and_records_interventions() {
        let mut state = RunState::new(
            ".".into(),
            "main".into(),
            "abc".into(),
            "task".into(),
            crate::config::Config::default(),
        );
        let (_, body) = prepare(
            &mut state,
            "fix: retries",
            "日本語の説明を書きます。 token=secret",
        );
        assert_eq!(body, NEUTRAL_BODY);
        assert!(!state.events.is_empty());
        state.config.graph.github_text_guard = false;
        let (_, body) = prepare(
            &mut state,
            "fix: retries",
            "日本語の説明を書きます。 token=secret",
        );
        assert!(body.contains("日本語"));
        assert!(!body.contains("secret"));
        assert!(
            check("fix: retries", &body)
                .iter()
                .all(|v| *v != Violation::SensitiveData)
        );
    }

    #[test]
    fn github_text_fixed_fallback_passes() {
        assert!(check(NEUTRAL_TITLE, NEUTRAL_BODY).is_empty());
    }
}

#[cfg(test)]
mod review_round_tests {
    use super::*;

    #[test]
    fn latin_script_non_english_is_flagged() {
        assert!(check("Corregir errores", "fix: retry").contains(&Violation::TitleLanguage));
        assert!(
            check(
                "fix: retries",
                "Este cambio agrega reintentos para solicitudes fallidas."
            )
            .contains(&Violation::BodyLanguage)
        );
        assert!(
            check(
                "fix: retry failed requests",
                "Adds retries for failed requests."
            )
            .is_empty()
        );
    }

    #[test]
    fn quoted_json_credentials_are_sensitive() {
        let body = "Example: {\"password\": \"hunter2\"}";
        assert!(check("t", body).contains(&Violation::SensitiveData));
        assert!(!crate::scrub::scrub(body, &Identity::default()).contains("hunter2"));
    }
}

#[cfg(test)]
mod quoted_value_tests {
    use crate::scrub::{Identity, scrub};

    #[test]
    fn quoted_values_are_redacted_whole() {
        for v in ["correct horse battery staple", ",hunter2"] {
            let out = scrub(
                &format!("{{\"password\": \"{v}\"}} ok"),
                &Identity::default(),
            );
            assert!(!out.contains("horse") && !out.contains("hunter2"), "{out}");
            assert!(out.ends_with("ok"), "{out}");
        }
    }
}
