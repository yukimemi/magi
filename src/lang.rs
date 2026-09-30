//! One reading of `[graph] language`, shared by everything that has a
//! translated wording table.
//!
//! Only English and Japanese are translated. The setting has always accepted
//! a code or a name, so the Japanese spellings are matched loosely; anything
//! else falls back to English rather than shipping a guess.

/// Whether `language` names Japanese: `ja`, `jp`, `japanese` or `日本語`,
/// trimmed and case-insensitive.
pub fn is_japanese(language: &str) -> bool {
    let l = language.trim();
    l.eq_ignore_ascii_case("ja")
        || l.eq_ignore_ascii_case("jp")
        || l.eq_ignore_ascii_case("japanese")
        || l.eq_ignore_ascii_case("日本語")
}

/// `[graph] language` of the repository at `repo`, read best-effort for
/// callers with no run (and so no config of their own). An unreadable config
/// is English: a hold reason must still get written.
pub fn of_repo(repo: &std::path::Path) -> String {
    crate::config::Config::discover(repo, None)
        .map(|(c, _)| c.graph.language)
        .unwrap_or_else(|_| "en".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_japanese_spelling_is_accepted() {
        for l in ["ja", "JA", "jp", "Japanese", " japanese ", "日本語"] {
            assert!(is_japanese(l), "{l:?}");
        }
    }

    #[test]
    fn everything_else_is_not_japanese() {
        for l in ["en", "", "de", "fr", "jap"] {
            assert!(!is_japanese(l), "{l:?}");
        }
    }
}
