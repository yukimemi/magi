//! What a graft round was asked to port and what the fixer says it did, for
//! the report.
//!
//! New runs keep both in [`GraftRecord`]. An older run kept only the prompt and
//! the CLI's output under `artifacts/`, so [`recover`] reads them back through
//! the same per-CLI answer extraction a live call uses, never the raw event
//! stream. The decisions ([`parse_prompt`], [`adoption_of`], [`standing`]) are
//! pure; only [`recover`] touches the disk.
//!
//! The fixer's report is a claim. A strength counts as adopted only when the
//! report names it by id or by its text; anything else is `unreported`, and a
//! record with no report at all says `unknown`, never "applied".

use std::path::Path;

use serde::Serialize;

use crate::config::AgentKind;
use crate::run::{GraftAdoption, GraftRejection, GraftStrength, RunState};
use crate::verdict::{self, FixReport};

/// Largest artifact read back, and characters kept of prose.
const MAX_ARTIFACT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PROSE: usize = 20_000;

/// A detail that could not be shown, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unavailable {
    /// Which detail: `requested`, `adoption`, `diff`.
    pub what: &'static str,
    /// Why, in words.
    pub reason: String,
}

/// What was recovered for a historical record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Strengths parsed from the stored prompt.
    pub requested: Vec<GraftStrength>,
    /// The fixer's report parsed from the stored output.
    pub adoption: Option<GraftAdoption>,
    /// What stayed unknown.
    pub unavailable: Vec<Unavailable>,
}

/// Where one strength stands against the fixer's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Standing {
    /// The report names it as ported.
    Adopted,
    /// The report declines it, with the reason.
    Rejected(String),
    /// A report exists and does not mention it.
    Unreported,
}

fn clip(s: &str) -> String {
    s.chars().take(MAX_PROSE).collect()
}

/// The fixer's reply as a stored adoption, or why it holds none. The text
/// before the report's fence is the reasoning.
pub fn adoption_of(text: &str) -> (Option<GraftAdoption>, Option<String>) {
    match verdict::extract_json::<FixReport>(text) {
        Ok(r) => {
            let reasoning = text
                .rfind("```json")
                .map_or(text, |i| &text[..i])
                .trim()
                .to_owned();
            (
                Some(GraftAdoption {
                    addressed: r.addressed,
                    rejected: r
                        .rejected
                        .into_iter()
                        .map(|x| GraftRejection {
                            id: x.id,
                            why: x.why,
                        })
                        .collect(),
                    notes: clip(&r.notes),
                    reasoning: clip(&reasoning),
                }),
                None,
            )
        }
        Err(e) => {
            let why = e.to_string();
            let first: String = why.lines().next().unwrap_or("").chars().take(200).collect();
            (None, Some(format!("no usable report: {first}")))
        }
    }
}

/// The strengths listed in a graft prompt. Items without an `[A1]` tag (a
/// prompt from before ids) get `A1`-style ids in order.
pub fn parse_prompt(prompt: &str) -> Vec<GraftStrength> {
    let mut out = Vec::new();
    let mut label: Option<char> = None;
    let mut n = 0;
    for line in prompt.lines() {
        if let Some(rest) = line.strip_prefix("## From candidate ") {
            label = rest.chars().next();
            n = 0;
        } else if line.starts_with("# ") && !line.starts_with("# Strengths") {
            label = None;
        } else if let (Some(l), Some(item)) = (label, line.strip_prefix("- ")) {
            n += 1;
            let tagged = item
                .strip_prefix('[')
                .and_then(|r| r.split_once("] "))
                .filter(|(id, _)| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()));
            let (id, text) = match tagged {
                Some((id, text)) => (id.to_owned(), text),
                None => (format!("{l}{n}"), item),
            };
            out.push(GraftStrength {
                id,
                source: l,
                text: text.trim().to_owned(),
            });
        }
    }
    out
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Does the fixer's entry refer to this strength, by id or by its text?
fn refers(entry: &str, s: &GraftStrength) -> bool {
    let first = entry
        .trim_start_matches(['[', '(', ' ', '`'])
        .split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap_or("");
    if first.eq_ignore_ascii_case(&s.id) {
        return true;
    }
    let (e, t) = (squash(entry), squash(&s.text));
    e.len() >= 12 && t.len() >= 12 && (e.contains(&t) || t.contains(&e))
}

/// Where `s` stands against the report. A rejection wins over an adoption of
/// the same strength: the report contradicting itself must not read as done.
pub fn standing(s: &GraftStrength, a: &GraftAdoption) -> Standing {
    if let Some(r) = a.rejected.iter().find(|r| refers(&r.id, s)) {
        return Standing::Rejected(r.why.clone());
    }
    if a.addressed.iter().any(|x| refers(x, s)) {
        Standing::Adopted
    } else {
        Standing::Unreported
    }
}

fn read_capped(path: &Path) -> Result<String, &'static str> {
    let meta = std::fs::metadata(path).map_err(|_| "artifact_missing")?;
    if meta.len() > MAX_ARTIFACT_BYTES {
        return Err("unreadable_output");
    }
    std::fs::read_to_string(path).map_err(|_| "unreadable_output")
}

/// The pure half of [`recover`]: parse whatever text was found.
pub fn details(
    kind: Option<AgentKind>,
    prompt: Result<&str, &'static str>,
    out: Result<&str, &'static str>,
) -> Recovered {
    let mut rec = Recovered::default();
    match prompt {
        Ok(p) => {
            rec.requested = parse_prompt(p);
            if rec.requested.is_empty() {
                rec.unavailable.push(Unavailable {
                    what: "requested",
                    reason: "the stored prompt lists no strengths".to_owned(),
                });
            }
        }
        Err(why) => rec.unavailable.push(Unavailable {
            what: "requested",
            reason: why.to_owned(),
        }),
    }
    match (kind, out) {
        (_, Err(why)) => rec.unavailable.push(Unavailable {
            what: "adoption",
            reason: why.to_owned(),
        }),
        (None, Ok(_)) => rec.unavailable.push(Unavailable {
            what: "adoption",
            reason: "the graft agent is no longer in the roster, so its output cannot be read"
                .to_owned(),
        }),
        (Some(k), Ok(raw)) => {
            let (adoption, err) = adoption_of(&crate::agent::answer_text(k, raw));
            match adoption {
                Some(a) => rec.adoption = Some(a),
                None => rec.unavailable.push(Unavailable {
                    what: "adoption",
                    reason: err.unwrap_or_else(|| "unreadable_output".to_owned()),
                }),
            }
        }
    }
    rec
}

/// Read back the pieces a record from before `requested` / `adoption` lacks.
/// The output file is chosen only when it provably belongs to the agent the
/// record names: the last graft handover's target, else the first attempt.
pub fn recover(state: &RunState, artifacts: &Path) -> Recovered {
    let Some(g) = &state.graft else {
        return Recovered::default();
    };
    let need_requested = g.requested.is_empty();
    let need_adoption = g.adoption.is_none() && g.adoption_error.is_none();
    if !need_requested && !need_adoption {
        return Recovered::default();
    }
    let last_to = state
        .handovers
        .iter()
        .rfind(|h| h.node == "graft")
        .map(|h| h.to.as_str());
    let stem = match (&g.agent, last_to) {
        (None, _) => None,
        (Some(a), Some(to)) => (a == to).then(|| format!("graft-{a}")),
        (Some(_), None) => Some("graft".to_owned()),
    };
    let prompt = read_capped(&artifacts.join("graft.prompt.md"));
    let out = match &stem {
        Some(s) => read_capped(&artifacts.join(format!("{s}.out"))),
        None => Err("artifact_missing"),
    };
    let kind = g
        .agent
        .as_deref()
        .and_then(|a| state.config.agents.iter().find(|s| s.id == a))
        .map(|s| s.kind);
    let mut rec = details(
        kind,
        prompt.as_deref().map_err(|e| *e),
        out.as_deref().map_err(|e| *e),
    );
    if !need_requested {
        rec.requested.clear();
        rec.unavailable.retain(|u| u.what != "requested");
    }
    if !need_adoption {
        rec.adoption = None;
        rec.unavailable.retain(|u| u.what != "adoption");
    }
    rec
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT: &str = "# Graft the runner-up strengths\n\nx\n\n# Strengths to port\n\n## From candidate A (branch `b`, read-only)\n\n- [A1] Build Persona directly in the test\n- [A2] Cache the lookup\n\n## From candidate C (branch `c`, read-only)\n\n- plain legacy item\n\n# Rules\n\n- not a strength\n";

    #[test]
    fn prompt_items_get_ids_tagged_or_numbered() {
        let v = parse_prompt(PROMPT);
        let ids: Vec<_> = v.iter().map(|s| (s.id.as_str(), s.source)).collect();
        assert_eq!(ids, [("A1", 'A'), ("A2", 'A'), ("C1", 'C')]);
        assert_eq!(v[0].text, "Build Persona directly in the test");
    }

    #[test]
    fn a_reply_without_a_report_holds_no_adoption() {
        let (a, e) = adoption_of("I did some things.");
        assert!(a.is_none());
        assert!(e.unwrap().starts_with("no usable report"));
    }

    #[test]
    fn requested_is_not_adopted_unless_the_report_says_so() {
        let reply = "Ported the test change.\n\n```json\n{\"addressed\":[\"A1\"],\"rejected\":[{\"id\":\"A2\",\"why\":\"too big\"}],\"notes\":\"n\"}\n```";
        let (a, _) = adoption_of(reply);
        let a = a.unwrap();
        assert_eq!(a.reasoning, "Ported the test change.");
        let v = parse_prompt(PROMPT);
        assert_eq!(standing(&v[0], &a), Standing::Adopted);
        assert_eq!(standing(&v[1], &a), Standing::Rejected("too big".into()));
        assert_eq!(standing(&v[2], &a), Standing::Unreported);
    }

    #[test]
    fn a_strength_is_matched_by_its_text_when_the_id_is_missing() {
        let a = GraftAdoption {
            addressed: vec!["Build Persona directly in the test".to_owned()],
            ..GraftAdoption::default()
        };
        let v = parse_prompt(PROMPT);
        assert_eq!(standing(&v[0], &a), Standing::Adopted);
        assert_eq!(standing(&v[1], &a), Standing::Unreported);
    }

    #[test]
    fn a_contradictory_report_does_not_read_as_adopted() {
        let a = GraftAdoption {
            addressed: vec!["A1".into()],
            rejected: vec![GraftRejection {
                id: "A1".into(),
                why: "no".into(),
            }],
            ..GraftAdoption::default()
        };
        assert_eq!(
            standing(&parse_prompt(PROMPT)[0], &a),
            Standing::Rejected("no".into())
        );
    }

    #[test]
    fn details_extracts_the_answer_through_the_cli_shape_not_the_raw_stream() {
        let inner = "why\n\n```json\n{\"addressed\":[\"A1\"],\"notes\":\"ok\"}\n```";
        let out = serde_json::json!({"result": inner, "session_id": "s"}).to_string();
        let r = details(Some(AgentKind::Claude), Ok(PROMPT), Ok(&out));
        assert_eq!(r.requested.len(), 3);
        assert_eq!(r.adoption.unwrap().addressed, ["A1"]);
        assert!(r.unavailable.is_empty());
    }

    #[test]
    fn missing_and_unreadable_pieces_are_named() {
        let r = details(
            Some(AgentKind::Claude),
            Err("artifact_missing"),
            Ok("not json at all"),
        );
        assert!(r.requested.is_empty());
        let whats: Vec<_> = r.unavailable.iter().map(|u| u.what).collect();
        assert_eq!(whats, ["requested", "adoption"]);
        let r = details(None, Ok(PROMPT), Ok("x"));
        assert!(r.adoption.is_none());
        assert_eq!(r.unavailable[0].what, "adoption");
    }
    fn legacy_state(agent: &str, handover_to: Option<&str>) -> RunState {
        crate::run::pin_test_home();
        let mut s = RunState::new(
            std::path::PathBuf::from("/repo/magi"),
            "main".to_owned(),
            "0123456789abcdef".to_owned(),
            "task".to_owned(),
            crate::config::Config::default(),
        );
        s.graft = Some(crate::run::GraftRecord {
            pre: "p".into(),
            post: Some("q".into()),
            lines: Some(1),
            limit: Some(40),
            agent: Some(agent.into()),
            outcome: crate::run::GraftOutcome::Applied,
            requested: Vec::new(),
            adoption: None,
            adoption_error: None,
        });
        if let Some(to) = handover_to {
            s.handovers.push(crate::run::Handover {
                at: jiff::Timestamp::now(),
                node: "graft".into(),
                seat: "graft".into(),
                from: "x".into(),
                to: to.into(),
                reason: "r".into(),
            });
        }
        s
    }

    #[test]
    fn recover_reads_the_handover_target_output_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("graft.prompt.md"), PROMPT).unwrap();
        let inner = "ok\n\n```json\n{\"addressed\":[\"A1\"],\"notes\":\"n\"}\n```";
        std::fs::write(
            dir.path().join("graft-astra.out"),
            serde_json::json!({"result": inner}).to_string(),
        )
        .unwrap();
        // The first attempt's output must not be taken for the final agent's.
        std::fs::write(dir.path().join("graft.out"), "{\"result\":\"stale\"}").unwrap();
        let mut s = legacy_state("astra", Some("astra"));
        s.config.agents =
            vec![toml::from_str("id = 'astra'\nkind = 'claude'").expect("agent spec")];
        let r = recover(&s, dir.path());
        assert_eq!(r.requested.len(), 3);
        assert_eq!(r.adoption.unwrap().addressed, ["A1"]);
    }

    #[test]
    fn recover_refuses_an_output_it_cannot_attribute() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("graft.prompt.md"), PROMPT).unwrap();
        std::fs::write(dir.path().join("graft.out"), "{}").unwrap();
        // The record names `sol` but the last handover went to `astra`.
        let s = legacy_state("sol", Some("astra"));
        let r = recover(&s, dir.path());
        assert!(r.adoption.is_none());
        assert!(r.unavailable.iter().any(|u| u.what == "adoption"));
        // No artifacts at all.
        let empty = tempfile::tempdir().unwrap();
        let r = recover(&legacy_state("sol", None), empty.path());
        assert_eq!(r.unavailable.len(), 2);
        assert_eq!(r.unavailable[0].reason, "artifact_missing");
    }
}
