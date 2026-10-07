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
];

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
    if foreign_words(text) {
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
            assert_eq!(check("fix: retries", body), vec![Violation::BodyLanguage], "{body}");
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
