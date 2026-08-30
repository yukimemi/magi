//! Blindness: label assignment, attribution stripping, and leak detection.
//!
//! A judge that knows which model wrote a candidate stops grading the patch and
//! starts voting on the model's reputation. Three things keep that from
//! happening:
//!
//! 1. **Labels.** Candidates are presented as `A`/`B`/`C`, assigned by a seeded
//!    shuffle, and each judge sees them in its own order so position carries no
//!    signal either.
//! 2. **Stripping.** Commit messages and candidate summaries lose their
//!    attribution trailers — both at write time (a per-worktree `commit-msg`
//!    hook) and at presentation time (this module). Belt and braces: the hook
//!    can be bypassed with `--no-verify`, the presentation filter cannot.
//! 3. **Leak detection.** The patch body is scanned for vendor-identifying
//!    text. Blanket redaction there would corrupt the artifact under judgement,
//!    so the policy is configurable and defaults to recording the leak.
//!
//! magi itself is the facilitator, which is the structural reason this works:
//! there is no moderator agent that *could* leak an author, because the
//! moderator is code that never learns anything it does not print.
use crate::config::{Blind, LeakPolicy};
use crate::rng::SplitMix64;

/// A vendor token found in material shown to judges.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Leak {
    /// Where it was found, e.g. `candidate B patch`.
    pub site: String,
    /// The token, as configured.
    pub token: String,
    /// How many times it occurred.
    pub count: usize,
}

/// Label for candidate index `i` after the seeded shuffle.
///
/// Returns one label per candidate: `labels[i]` is the label candidate `i` is
/// presented under.
pub fn assign_labels(n: usize, seed: u64) -> Vec<char> {
    let mut pool: Vec<char> = (0..n).map(label_char).collect();
    SplitMix64::new(seed).shuffle(&mut pool);
    pool
}

/// `0 -> 'A'`, `25 -> 'Z'`, then wraps with a digit suffix scheme that is still
/// unique but never reached in practice (`candidates` above 26 is nonsense).
fn label_char(i: usize) -> char {
    char::from(b'A' + (i % 26) as u8)
}

/// The order judge `j` sees the candidates in, as indices into the candidate
/// list.
pub fn presentation_order(n: usize, judge: usize, seed: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    SplitMix64::new(seed ^ crate::rng::fnv1a(&format!("judge-order-{judge}"))).shuffle(&mut order);
    order
}

/// Drop every line containing one of `patterns` (case-insensitive substring).
pub fn strip_attribution(text: &str, patterns: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let lowered = ascii_lower(line);
        if patterns
            .iter()
            .any(|p| !p.is_empty() && lowered.contains(&ascii_lower(p)))
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Count occurrences of each vendor token in `text`.
pub fn scan(site: &str, text: &str, tokens: &[String]) -> Vec<Leak> {
    let lowered = ascii_lower(text);
    tokens
        .iter()
        .filter(|t| !t.is_empty())
        .filter_map(|t| {
            let count = lowered.matches(&ascii_lower(t)).count();
            (count > 0).then(|| Leak {
                site: site.to_owned(),
                token: t.clone(),
                count,
            })
        })
        .collect()
}

/// Replace every vendor token with `[REDACTED]`, case-insensitively.
pub fn redact(text: &str, tokens: &[String]) -> String {
    const PLACEHOLDER: &str = "[REDACTED]";
    let mut out = text.to_owned();
    for t in tokens.iter().filter(|t| !t.is_empty()) {
        let needle = ascii_lower(t);
        // Scan the pre-replacement copy and build a new string, so the
        // placeholder is never itself scanned. Replacing in place and
        // restarting would loop forever on any token whose letters occur in
        // `PLACEHOLDER` — "codex" and "cursor" both do.
        let lowered = ascii_lower(&out);
        let mut result = String::with_capacity(out.len());
        let mut cursor = 0usize;
        while let Some(rel) = lowered[cursor..].find(&needle) {
            let at = cursor + rel;
            result.push_str(&out[cursor..at]);
            result.push_str(PLACEHOLDER);
            cursor = at + t.len();
        }
        result.push_str(&out[cursor..]);
        out = result;
    }
    out
}

/// Sanitize prose written by a candidate (commit messages, summaries).
///
/// Always strips *and* redacts: prose has no structural value to preserve, and
/// it is where "Generated with X" actually shows up.
pub fn sanitize_prose(text: &str, cfg: &Blind) -> String {
    let stripped = strip_attribution(text, &cfg.strip_lines);
    redact(&stripped, &cfg.vendor_tokens).trim().to_owned()
}

/// Apply the configured leak policy to a patch body.
///
/// Returns the text to show the judges plus everything found.
pub fn sanitize_patch(site: &str, patch: &str, cfg: &Blind) -> (String, Vec<Leak>) {
    let leaks = scan(site, patch, &cfg.vendor_tokens);
    let text = match cfg.on_leak {
        LeakPolicy::Redact => redact(patch, &cfg.vendor_tokens),
        LeakPolicy::Warn | LeakPolicy::Fail => patch.to_owned(),
    };
    (text, leaks)
}

/// The `commit-msg` hook installed into every candidate worktree.
///
/// POSIX `sh` plus `sed`, which is what git runs hooks with on every platform
/// magi supports, git-for-windows included. `sed` is given a case-insensitive
/// expansion of each configured substring so the hook and
/// [`strip_attribution`] agree on what counts as attribution.
pub fn commit_msg_hook(patterns: &[String]) -> String {
    let mut script = String::from(
        "#!/bin/sh\n\
         # Installed by magi. Candidate history must not name its author:\n\
         # a judge that can read `Co-Authored-By:` is no longer blind.\n\
         set -e\n\
         msg=\"$1\"\n\
         tmp=\"${msg}.magi\"\n\
         sed \\\n",
    );
    for p in patterns.iter().filter(|p| !p.is_empty()) {
        script.push_str(&format!("  -e '/{}/d' \\\n", sed_ci_pattern(p)));
    }
    script.push_str(
        "  \"$msg\" > \"$tmp\"\n\
         mv \"$tmp\" \"$msg\"\n",
    );
    script
}

/// Turn a literal substring into a case-insensitive basic-regex, escaping the
/// metacharacters that matter inside a `sed` address.
fn sed_ci_pattern(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len() * 4);
    for ch in literal.chars() {
        if ch.is_ascii_alphabetic() {
            out.push('[');
            out.push(ch.to_ascii_uppercase());
            out.push(ch.to_ascii_lowercase());
            out.push(']');
        } else if matches!(ch, '.' | '*' | '[' | ']' | '^' | '$' | '\\' | '/') {
            out.push('\\');
            out.push(ch);
        } else if ch == '\'' {
            // Cannot appear inside the single-quoted sed expression.
            out.push('.');
        } else {
            out.push(ch);
        }
    }
    out
}

/// ASCII-only lowercase.
///
/// [`str::to_lowercase`] is Unicode-aware and can change a string's byte
/// length, which would invalidate the offsets [`redact`] splices at. Folding
/// only ASCII keeps every byte offset identical between the original and the
/// lowered copy.
fn ascii_lower(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.chars() {
        out.push(if b.is_ascii_uppercase() {
            b.to_ascii_lowercase()
        } else {
            b
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Blind {
        Blind::default()
    }

    #[test]
    fn labels_are_a_permutation_and_stable_for_a_seed() {
        let a = assign_labels(3, 99);
        let b = assign_labels(3, 99);
        assert_eq!(a, b);
        let mut sorted = a.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, ['A', 'B', 'C']);
    }

    #[test]
    fn each_judge_gets_its_own_presentation_order() {
        let orders: Vec<Vec<usize>> = (0..3).map(|j| presentation_order(3, j, 5)).collect();
        for o in &orders {
            let mut s = o.clone();
            s.sort_unstable();
            assert_eq!(s, [0, 1, 2]);
        }
        assert!(
            orders.iter().any(|o| *o != orders[0]),
            "three judges should not all see the same order: {orders:?}"
        );
    }

    #[test]
    fn trailers_are_stripped_case_insensitively() {
        let msg = "Add retry\n\nBody text.\nco-authored-by: Claude <noreply@anthropic.com>\n\
                   Co-Authored-By: Someone\nGenerated with the thing\nkeep me\n";
        let out = strip_attribution(msg, &cfg().strip_lines);
        assert!(out.contains("Add retry"));
        assert!(out.contains("keep me"));
        assert!(!out.to_lowercase().contains("co-authored-by"));
        assert!(!out.contains("Generated with"));
    }

    #[test]
    fn prose_sanitizer_strips_then_redacts() {
        let out = sanitize_prose(
            "I used Claude to write this.\nCo-Authored-By: X\nDone.",
            &cfg(),
        );
        assert!(!out.to_lowercase().contains("claude"), "{out}");
        assert!(out.contains("[REDACTED]"));
        assert!(out.contains("Done."));
    }

    #[test]
    fn redact_preserves_surrounding_bytes_with_multibyte_text() {
        let tokens = vec!["claude".to_owned()];
        let out = redact("日本語 CLAUDE で書いた 🤖", &tokens);
        assert_eq!(out, "日本語 [REDACTED] で書いた 🤖");
    }

    #[test]
    fn redact_terminates_when_replacement_contains_no_token() {
        let tokens = vec!["a".to_owned()];
        assert_eq!(redact("aaa", &tokens), "[REDACTED][REDACTED][REDACTED]");
    }

    #[test]
    fn scan_counts_without_modifying() {
        let leaks = scan(
            "candidate B patch",
            "Claude and claude and Gemini",
            &cfg().vendor_tokens,
        );
        let claude = leaks.iter().find(|l| l.token == "claude").unwrap();
        assert_eq!(claude.count, 2);
        assert_eq!(claude.site, "candidate B patch");
        assert!(leaks.iter().any(|l| l.token == "gemini"));
    }

    #[test]
    fn warn_policy_leaves_the_patch_intact() {
        let mut c = cfg();
        c.on_leak = LeakPolicy::Warn;
        let patch = "+// written by claude\n";
        let (text, leaks) = sanitize_patch("candidate A patch", patch, &c);
        assert_eq!(text, patch, "a warn must not rewrite the diff");
        assert!(!leaks.is_empty());
    }

    #[test]
    fn redact_policy_rewrites_the_patch() {
        let mut c = cfg();
        c.on_leak = LeakPolicy::Redact;
        let (text, leaks) = sanitize_patch("candidate A patch", "+// by claude\n", &c);
        assert!(text.contains("[REDACTED]"));
        assert_eq!(leaks.len(), 1);
    }

    #[test]
    fn hook_script_is_case_insensitive_sh() {
        let script = commit_msg_hook(&cfg().strip_lines);
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains("[Cc][Oo]-[Aa][Uu][Tt][Hh][Oo][Rr][Ee][Dd]-[Bb][Yy]:"));
        assert!(script.contains("mv \"$tmp\" \"$msg\""));
    }

    #[test]
    fn sed_pattern_escapes_metacharacters() {
        assert_eq!(sed_ci_pattern("a.b"), "[Aa]\\.[Bb]");
        assert_eq!(sed_ci_pattern("x/y"), "[Xx]\\/[Yy]");
        assert_eq!(sed_ci_pattern("\u{1f916}"), "\u{1f916}");
    }
}
