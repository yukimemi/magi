//! Structured answers extracted from free-form agent output.
//!
//! Agents are asked to end with a fenced `json` block. They mostly do, and
//! sometimes they narrate afterwards, emit two blocks, or wrap the object in
//! prose. [`extract_json`] therefore scans for *every* balanced top-level
//! object in the text and returns the last one that deserializes into the type
//! the caller wants, rather than trusting a single regex to find the right one.
use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A judge's independent ranking of the candidates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ranking {
    /// Labels, best first. Must be a permutation of the presented labels.
    pub ranking: Vec<String>,
    /// Per-label justification.
    #[serde(default)]
    pub reasons: BTreeMap<String, String>,
    /// Self-reported confidence, 1-5.
    #[serde(default)]
    pub confidence: Option<u8>,
}

impl Ranking {
    /// The judge's first choice.
    pub fn top(&self) -> Option<&str> {
        self.ranking.first().map(String::as_str)
    }

    /// Reject a ranking that is not a permutation of `labels`, so a malformed
    /// verdict is retried instead of silently skewing the tally.
    pub fn validate(&self, labels: &[char]) -> Result<()> {
        let mut got: Vec<char> = self
            .ranking
            .iter()
            .filter_map(|s| s.trim().chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        got.sort_unstable();
        got.dedup();
        let mut want: Vec<char> = labels.to_vec();
        want.sort_unstable();
        if got != want {
            bail!(
                "ranking {:?} is not a permutation of the candidate labels {:?}",
                self.ranking,
                labels
            );
        }
        Ok(())
    }

    /// Normalise labels to single uppercase characters.
    pub fn normalized(&self) -> Vec<char> {
        self.ranking
            .iter()
            .filter_map(|s| s.trim().chars().next())
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }
}

/// A judge's final vote, collected privately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FinalVote {
    /// The winning label, in this judge's view.
    pub vote: String,
    /// Why.
    #[serde(default)]
    pub reason: String,
}

impl FinalVote {
    /// The voted label as a single uppercase char.
    pub fn label(&self) -> Option<char> {
        self.vote
            .trim()
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
    }
}

/// How bad a review finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Cosmetic.
    Nit,
    /// Should fix, does not block.
    Minor,
    /// Should fix before merge.
    Major,
    /// Must fix before merge.
    Blocker,
}

impl Severity {
    /// Does this finding hold the merge?
    pub fn blocks(self) -> bool {
        matches!(self, Self::Major | Self::Blocker)
    }
}

/// One review finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Assigned by magi after parsing, e.g. `R1-1-2`. Never trusted from the
    /// agent, because the fixer's adoption report is keyed by it.
    #[serde(default)]
    pub id: String,
    /// How bad.
    pub severity: Severity,
    /// File it concerns.
    #[serde(default)]
    pub file: Option<String>,
    /// Line it concerns.
    #[serde(default)]
    pub line: Option<u32>,
    /// One-line summary.
    pub title: String,
    /// The argument.
    #[serde(default)]
    pub detail: String,
}

/// A reviewer's report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    /// Findings, worst first is conventional but not required.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Reviewer's overall verdict prose.
    #[serde(default)]
    pub summary: String,
}

/// The fixer's response to a round of findings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixReport {
    /// Finding ids that were acted on.
    #[serde(default)]
    pub addressed: Vec<String>,
    /// Finding ids that were deliberately not acted on, with the reason.
    #[serde(default)]
    pub rejected: Vec<Rejection>,
    /// What changed.
    #[serde(default)]
    pub notes: String,
}

/// A finding the fixer declined, and why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rejection {
    /// Finding id.
    pub id: String,
    /// Argument for not acting.
    #[serde(default)]
    pub why: String,
}

/// A judge's position during deliberation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// Where the judge currently stands.
    #[serde(default)]
    pub tentative: Option<String>,
}

/// Extract the last balanced JSON object in `text` that parses as `T`.
///
/// Handles fenced blocks, trailing prose, and multiple objects. Strings and
/// escapes are tracked so a `}` inside a string literal does not close an
/// object early.
pub fn extract_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escaped = false;
        let mut j = i;
        while j < bytes.len() {
            let c = bytes[j];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else {
                match c {
                    b'"' => in_str = true,
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            spans.push((i, j + 1));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            j += 1;
        }
        // Skip past this object's opening brace either way; a truncated object
        // must not make the scan quadratic on long transcripts.
        i = if depth == 0 && j < bytes.len() {
            j + 1
        } else {
            i + 1
        };
    }

    let mut last_err = None;
    for (start, end) in spans.iter().rev() {
        match serde_json::from_str::<T>(&text[*start..*end]) {
            Ok(v) => return Ok(v),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => bail!("no JSON object in the reply matched the expected shape: {e}"),
        None => bail!("the reply contained no JSON object"),
    }
}

/// Pull the text after a `## <heading>` marker, to the end or the next heading.
///
/// Used for the prose sections agents are asked to emit alongside their JSON.
pub fn section(text: &str, heading: &str) -> Option<String> {
    let want = heading.to_ascii_lowercase();
    let mut out: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("##") {
            let name = rest.trim_start_matches('#').trim().to_ascii_lowercase();
            if name == want {
                out = Some(String::new());
                continue;
            }
            if out.is_some() {
                break;
            }
            continue;
        }
        if let Some(buf) = out.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    out.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_block_is_found() {
        let text = "Here is my verdict.\n\n```json\n{\"ranking\":[\"B\",\"A\"]}\n```\n";
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.top(), Some("B"));
    }

    #[test]
    fn last_matching_object_wins_over_an_earlier_example() {
        let text = concat!(
            "The format is {\"ranking\":[\"X\"]} for illustration.\n",
            "```json\n{\"ranking\":[\"C\",\"A\",\"B\"],\"confidence\":4}\n```\n",
            "Happy to elaborate.\n"
        );
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.normalized(), ['C', 'A', 'B']);
        assert_eq!(r.confidence, Some(4));
    }

    #[test]
    fn braces_inside_strings_do_not_close_the_object() {
        let text = r#"{"ranking":["A"],"reasons":{"A":"uses format!(\"{}\", x) safely}"}}"#;
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.top(), Some("A"));
        assert!(r.reasons["A"].contains("format!"));
    }

    #[test]
    fn objects_of_the_wrong_shape_are_skipped() {
        let text = concat!(
            "```json\n{\"ranking\":[\"A\",\"B\"]}\n```\n",
            "and some telemetry: {\"tokens\":123}\n"
        );
        let r: Ranking = extract_json(text).unwrap();
        assert_eq!(r.normalized(), ['A', 'B']);
    }

    #[test]
    fn no_json_is_an_error_not_a_default() {
        let err = extract_json::<Ranking>("I decline to produce JSON.").unwrap_err();
        assert!(err.to_string().contains("no JSON object"));
    }

    #[test]
    fn truncated_object_does_not_hang() {
        let err = extract_json::<Ranking>("{\"ranking\": [\"A\"").unwrap_err();
        assert!(err.to_string().contains("no JSON object"));
    }

    #[test]
    fn ranking_validation_rejects_a_non_permutation() {
        let r = Ranking {
            ranking: vec!["A".to_owned(), "A".to_owned()],
            reasons: BTreeMap::new(),
            confidence: None,
        };
        assert!(r.validate(&['A', 'B', 'C']).is_err());

        let r = Ranking {
            ranking: vec!["c".to_owned(), "B".to_owned(), "A".to_owned()],
            reasons: BTreeMap::new(),
            confidence: None,
        };
        r.validate(&['A', 'B', 'C']).expect("case is normalised");
        assert_eq!(r.normalized(), ['C', 'B', 'A']);
    }

    #[test]
    fn final_vote_label_is_normalised() {
        let v: FinalVote = extract_json(r#"{"vote":" b ","reason":"tests"}"#).unwrap();
        assert_eq!(v.label(), Some('B'));
    }

    #[test]
    fn severity_blocking_is_major_and_up() {
        assert!(Severity::Blocker.blocks());
        assert!(Severity::Major.blocks());
        assert!(!Severity::Minor.blocks());
        assert!(!Severity::Nit.blocks());
        assert!(Severity::Blocker > Severity::Nit);
    }

    #[test]
    fn review_parses_with_optional_fields_missing() {
        let r: Review = extract_json(
            r#"{"findings":[{"severity":"blocker","title":"panics on empty input"}]}"#,
        )
        .unwrap();
        assert_eq!(r.findings.len(), 1);
        assert!(r.findings[0].file.is_none());
        assert_eq!(r.findings[0].id, "");
    }

    #[test]
    fn fix_report_parses_rejections() {
        let f: FixReport = extract_json(
            r#"{"addressed":["R1-1-1"],"rejected":[{"id":"R1-2-1","why":"not reachable"}]}"#,
        )
        .unwrap();
        assert_eq!(f.addressed, ["R1-1-1"]);
        assert_eq!(f.rejected[0].id, "R1-2-1");
    }

    #[test]
    fn sections_are_sliced_by_heading() {
        let text = "## SUMMARY\nchanged the retry loop.\nadded a test.\n\n## NOTES\nignore me\n";
        assert_eq!(
            section(text, "summary").unwrap(),
            "changed the retry loop.\nadded a test."
        );
        assert_eq!(section(text, "notes").unwrap(), "ignore me");
        assert!(section(text, "missing").is_none());
    }
}
