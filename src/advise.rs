//! Data and pure logic for the design-deliberation stage.
//!
//! Gathering the advisor proposals and running the synthesis seat is
//! [`crate::graph::Runner::advise`]'s job - a graph node, wired the same way
//! `judge` and `review` already are, run once a run has a settled task
//! instruction and before any implementer touches the repository. This
//! module only holds what that node produces ([`AdvisorRecord`],
//! [`Advice`]) and the pure, process-free logic for reading it back
//! ([`Advice::proposals`], [`apply_reflection`]) - split out so both are unit
//! testable without spawning an agent.
use serde::{Deserialize, Serialize};

use crate::verdict::Proposal;

/// How much of an advisor's proposal the synthesis brief appears to carry
/// forward.
///
/// Approximate by construction: there is no explicit data saying which
/// sentence of a blended brief came from which seat, only whether the
/// brief's own text names the seat or carries recognisable traces of its
/// proposal. See [`apply_reflection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reflection {
    /// The seat produced no proposal at all (crashed, timed out, or answered
    /// with nothing a [`Proposal`] could be parsed out of) - nothing to
    /// reflect.
    Absent,
    /// A proposal exists, but the synthesis brief carries little or no
    /// recognisable trace of it.
    Faint,
    /// The synthesis brief names this seat outright, or carries enough of
    /// its proposal's own wording to be a clear match.
    Strong,
}

impl Default for Reflection {
    /// The safe reading for a record nothing has classified yet: until
    /// [`apply_reflection`] runs, or when there never was a synthesis to
    /// compare against, "nothing shown as reflected" is correct whether or
    /// not a proposal exists.
    fn default() -> Self {
        Self::Absent
    }
}

/// One advisor seat's outcome, kept even on failure so a synthesis that only
/// had some of the seats to work with is not a mystery later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvisorRecord {
    /// Seat name, e.g. `advisor-1`.
    pub seat: String,
    /// Agent id occupying the seat.
    pub agent: String,
    /// The proposal, when the seat produced a usable one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<Proposal>,
    /// Why there is no proposal, when there is not one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// How much of this proposal the synthesis appears to carry forward.
    /// `#[serde(default)]` so a record predating this field, or one written
    /// before [`apply_reflection`] ran, reads as [`Reflection::Absent`]
    /// rather than failing to deserialize.
    #[serde(default)]
    pub reflection: Reflection,
}

impl AdvisorRecord {
    /// A seat that produced a usable, validated proposal.
    pub fn proposed(seat_num: usize, agent: String, proposal: Proposal, duration_ms: u64) -> Self {
        Self {
            seat: format!("advisor-{seat_num}"),
            agent,
            proposal: Some(proposal),
            error: None,
            duration_ms,
            reflection: Reflection::Absent,
        }
    }

    /// A seat that crashed, timed out, or answered with nothing a
    /// [`Proposal`] could be read out of.
    pub fn failed(seat_num: usize, agent: String, error: String) -> Self {
        Self {
            seat: format!("advisor-{seat_num}"),
            agent,
            proposal: None,
            error: Some(error),
            duration_ms: 0,
            reflection: Reflection::Absent,
        }
    }
}

/// The whole design-deliberation stage: one record per advisor seat, plus
/// the synthesis blended from whichever seats produced a usable proposal.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Advice {
    /// One record per advisor seat asked.
    pub records: Vec<AdvisorRecord>,
    /// The blended design brief carried in the implementer's prompt, when a
    /// synthesis seat produced one. `None` when no advisor produced a usable
    /// proposal, or the synthesis seat itself failed - the implementer then
    /// gets the task instruction alone, same as a run with `[graph] advise`
    /// off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesis: Option<String>,
}

impl Advice {
    /// The seats that produced a usable proposal, in seat order.
    pub fn proposals(&self) -> Vec<(&str, &Proposal)> {
        self.records
            .iter()
            .filter_map(|r| r.proposal.as_ref().map(|p| (r.seat.as_str(), p)))
            .collect()
    }
}

/// Tokens shorter than this are common enough to turn up in any prose by
/// chance ("the", "with", "into"), which would inflate every proposal's
/// overlap score regardless of whether the synthesis actually drew on it.
const MIN_TOKEN_LEN: usize = 5;

/// Fraction of an advisor's own significant words that must show up in the
/// synthesis text, verbatim, before [`classify`] counts the overlap alone as
/// [`Reflection::Strong`]. An explicit mention of the seat's own name always
/// counts on its own, regardless of this ratio - see [`classify`].
const STRONG_OVERLAP: f64 = 0.25;

/// Lowercased, deduplicated words of at least [`MIN_TOKEN_LEN`] characters.
fn tokens(text: &str) -> Vec<String> {
    let mut words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.len() >= MIN_TOKEN_LEN)
        .collect();
    words.sort();
    words.dedup();
    words
}

/// One record's [`Reflection`] against a lowercased synthesis text.
///
/// Two signals, checked in order: an outright mention of the seat's own name
/// (the synthesis prompt asks the seat to attribute ideas that way, so this
/// is the strong, unambiguous case), then a fallback word-overlap ratio over
/// the proposal's own significant vocabulary (`approach`, `key_tradeoff`,
/// `touches`) for a synthesis that paraphrased without naming anyone.
fn classify(record: &AdvisorRecord, synthesis_lower: &str) -> Reflection {
    let Some(proposal) = record.proposal.as_ref() else {
        return Reflection::Absent;
    };
    if synthesis_lower.is_empty() {
        return Reflection::Faint;
    }
    if synthesis_lower.contains(&record.seat.to_lowercase()) {
        return Reflection::Strong;
    }
    let mut words = tokens(&proposal.approach);
    words.extend(tokens(&proposal.key_tradeoff));
    for touch in &proposal.touches {
        words.extend(tokens(touch));
    }
    words.sort();
    words.dedup();
    if words.is_empty() {
        return Reflection::Faint;
    }
    let hits = words
        .iter()
        .filter(|w| synthesis_lower.contains(w.as_str()))
        .count();
    if (hits as f64) / (words.len() as f64) >= STRONG_OVERLAP {
        Reflection::Strong
    } else {
        Reflection::Faint
    }
}

/// Classify every record's [`Reflection`] against `advice.synthesis`, once
/// the synthesis text (or its absence) is known.
///
/// Idempotent and process-free: safe to call once after the synthesis call
/// settles, whatever it produced.
pub fn apply_reflection(advice: &mut Advice) {
    let synthesis_lower = advice
        .synthesis
        .as_deref()
        .unwrap_or_default()
        .to_lowercase();
    for record in &mut advice.records {
        record.reflection = classify(record, &synthesis_lower);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(approach: &str, key_tradeoff: &str, touches: &[&str]) -> Proposal {
        Proposal {
            approach: approach.to_owned(),
            key_tradeoff: key_tradeoff.to_owned(),
            risks: Vec::new(),
            touches: touches.iter().map(|s| (*s).to_owned()).collect(),
            why_not_naive: "because the naive version breaks under load".to_owned(),
        }
    }

    #[test]
    fn advice_proposals_skips_failed_records() {
        let advice = Advice {
            records: vec![
                AdvisorRecord::proposed(1, "a".to_owned(), proposal("do X", "t", &[]), 10),
                AdvisorRecord::failed(2, "b".to_owned(), "timed out".to_owned()),
            ],
            synthesis: None,
        };
        let proposals = advice.proposals();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].0, "advisor-1");
    }

    #[test]
    fn a_failed_seat_is_classified_absent_regardless_of_synthesis() {
        let record = AdvisorRecord::failed(1, "a".to_owned(), "crashed".to_owned());
        assert_eq!(
            classify(&record, "a synthesis that mentions advisor-1 by name"),
            Reflection::Absent
        );
    }

    #[test]
    fn an_explicit_seat_mention_is_strong_even_with_no_word_overlap() {
        let record = AdvisorRecord::proposed(
            1,
            "a".to_owned(),
            proposal("switch to polling", "latency", &["src/watch.rs"]),
            10,
        );
        let synthesis = "advisor-1 argued for a completely different rewrite.".to_lowercase();
        assert_eq!(classify(&record, &synthesis), Reflection::Strong);
    }

    #[test]
    fn strong_word_overlap_counts_without_a_seat_mention() {
        let record = AdvisorRecord::proposed(
            1,
            "a".to_owned(),
            proposal(
                "switch the poller to exponential backoff",
                "latency versus battery",
                &["src/watch.rs"],
            ),
            10,
        );
        let synthesis =
            "the plan settles on exponential backoff in the poller, touching src/watch.rs."
                .to_lowercase();
        assert_eq!(classify(&record, &synthesis), Reflection::Strong);
    }

    #[test]
    fn no_overlap_and_no_mention_is_faint_not_absent() {
        let record = AdvisorRecord::proposed(
            1,
            "a".to_owned(),
            proposal("switch to polling", "latency", &["src/watch.rs"]),
            10,
        );
        let synthesis = "the brief goes an entirely unrelated direction.".to_lowercase();
        assert_eq!(classify(&record, &synthesis), Reflection::Faint);
    }

    #[test]
    fn no_synthesis_at_all_is_faint_for_every_proposal() {
        let record = AdvisorRecord::proposed(1, "a".to_owned(), proposal("do X", "t", &[]), 10);
        assert_eq!(classify(&record, ""), Reflection::Faint);
    }

    #[test]
    fn apply_reflection_covers_every_record_including_failed_ones() {
        let mut advice = Advice {
            records: vec![
                AdvisorRecord::proposed(
                    1,
                    "a".to_owned(),
                    proposal("switch to polling", "latency", &["src/watch.rs"]),
                    10,
                ),
                AdvisorRecord::failed(2, "b".to_owned(), "timed out".to_owned()),
            ],
            synthesis: Some("advisor-1 argued for polling, which the brief adopts.".to_owned()),
        };
        apply_reflection(&mut advice);
        assert_eq!(advice.records[0].reflection, Reflection::Strong);
        assert_eq!(advice.records[1].reflection, Reflection::Absent);
    }
}
