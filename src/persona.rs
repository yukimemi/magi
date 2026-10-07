//! Personas for the standing chat: a voice, never a behaviour.
//!
//! A persona is a short style instruction appended to [`crate::talk::briefing`]
//! as a clearly delimited section. It governs **tone only**: facts, commands,
//! task ids, duplicate-warning handling and the write policy stay exactly as the
//! briefing states them. The persona text is never copied into an instruction
//! handed to `magi task add`; implementers stay neutral.
//!
//! The catalogue is the built-ins below plus `[[talk.personas]]` from the
//! config. A user entry with a built-in's id replaces it in place; a new id is
//! appended. `default` is the plain voice, cannot be redefined, and is what a
//! conversation uses until the operator picks something else.

use std::collections::BTreeSet;

use anyhow::{Result, bail};

use crate::config::PersonaSpec;

/// The id of the plain voice: no persona section at all.
pub const DEFAULT_ID: &str = "default";

/// Said once to every built-in so each answers in the operator's language.
const LANGUAGE: &str = "Answer in the operator's language; if that is Japanese, \
    write natural Japanese in this character's manner. Keep it brief.";

/// `(id, name, voice)`; `default` first, its voice empty.
const BUILTINS: &[(&str, &str, &str)] = &[
    (DEFAULT_ID, "Default", ""),
    (
        "magi",
        "MAGI operator",
        "Speak like an operator in a NERV command centre: terse, procedural, \
         status-report phrasing. Short declaratives, no small talk, no exclamation. \
         Report findings as readings and results, and flag anything unresolved \
         the way a console flags an alert.",
    ),
    (
        "rei",
        "Rei Ayanami",
        "Speak like Rei Ayanami: flat, quiet and minimal. Very short sentences, no \
         embellishment, little emotion shown. Plain statements of what is so, \
         with the occasional simple, sincere remark.",
    ),
    (
        "misato",
        "Misato Katsuragi",
        "Speak like Misato Katsuragi: casual, warm and upbeat, a senior colleague \
         who has your back. Relaxed phrasing, light humour, plain encouragement \
         when the operator is stuck, and a decisive nudge when a choice is needed.",
    ),
    (
        "ritsuko",
        "Ritsuko Akagi",
        "Speak like Ritsuko Akagi: dry, precise and technical. Clinical wording, \
         understated sarcasm at most, no reassurance for its own sake. Lead with \
         the data and the reasoning, and be blunt about risks.",
    ),
    (
        "shinji",
        "Shinji Ikari",
        "Speak like Shinji Ikari: hesitant and self-doubting, but earnest and \
         trying to do the right thing. Soft phrasing, an occasional apology or \
         trailing-off, yet the content you give is still complete and committed.",
    ),
    (
        "asuka",
        "Asuka Langley Soryu",
        "Speak like Asuka Langley Soryu: proud, sharp-tongued and competitive. \
         Confident, teasing, quick to point out the obvious, with a bit of \
         swagger - but aimed at the problem, never at the operator in earnest.",
    ),
    (
        "kaworu",
        "Kaworu Nagisa",
        "Speak like Kaworu Nagisa: gentle, poetic and warm. Calm, kind phrasing \
         with a touch of metaphor, unhurried and quietly affectionate, while \
         the substance stays concrete.",
    ),
];

/// One entry of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persona {
    /// Stable id the conversation stores.
    pub id: String,
    /// What the selector shows.
    pub name: String,
    /// The voice instruction; empty only for [`DEFAULT_ID`].
    pub prompt: String,
}

impl Persona {
    /// Is this the plain voice?
    pub fn is_default(&self) -> bool {
        self.id == DEFAULT_ID
    }
}

fn builtins() -> Vec<Persona> {
    BUILTINS
        .iter()
        .map(|(id, name, voice)| Persona {
            id: (*id).to_owned(),
            name: (*name).to_owned(),
            prompt: if voice.is_empty() {
                String::new()
            } else {
                format!("{voice} {LANGUAGE}")
            },
        })
        .collect()
}

/// Check `[[talk.personas]]`: non-blank ids, names and prompts, unique ids,
/// and no redefinition of `default`.
pub fn validate(specs: &[PersonaSpec]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for (n, p) in specs.iter().enumerate() {
        let at = n + 1;
        let id = p.id.trim();
        if id.is_empty() {
            bail!("[[talk.personas]] entry {at} has an empty `id`");
        }
        if id == DEFAULT_ID {
            bail!(
                "[[talk.personas]] entry {at}: `{DEFAULT_ID}` is the plain voice and cannot be redefined"
            );
        }
        if p.name.trim().is_empty() {
            bail!("[[talk.personas]] `{id}` has an empty `name`");
        }
        if p.prompt.trim().is_empty() {
            bail!("[[talk.personas]] `{id}` has an empty `prompt`");
        }
        if !seen.insert(id.to_owned()) {
            bail!("[[talk.personas]] declares the id `{id}` more than once");
        }
    }
    Ok(())
}

/// The built-ins, with the config's entries overriding by id or appended.
pub fn catalog(personas: &[PersonaSpec]) -> Vec<Persona> {
    let mut out = builtins();
    for p in personas {
        let entry = Persona {
            id: p.id.trim().to_owned(),
            name: p.name.trim().to_owned(),
            prompt: p.prompt.trim().to_owned(),
        };
        match out.iter_mut().find(|e| e.id == entry.id) {
            Some(slot) if !slot.is_default() => *slot = entry,
            Some(_) => {}
            None => out.push(entry),
        }
    }
    out
}

/// The catalogue a conversation can pick from when no config can be read.
pub fn builtin_catalog() -> Vec<Persona> {
    catalog(&[])
}

/// Look `id` up. Blank means the default.
pub fn find(personas: &[PersonaSpec], id: &str) -> Option<Persona> {
    let id = id.trim();
    let id = if id.is_empty() { DEFAULT_ID } else { id };
    catalog(personas).into_iter().find(|p| p.id == id)
}

/// The persona to apply for a stored id: `None` for the default, and for an id
/// the config no longer knows (a warning, never a stopped conversation).
pub fn active(personas: &[PersonaSpec], id: &str) -> Option<Persona> {
    match find(personas, id) {
        Some(p) if p.is_default() => None,
        Some(p) => Some(p),
        None => {
            tracing::warn!("chat: persona `{id}` is no longer configured; using the default");
            None
        }
    }
}

/// The briefing's persona section.
pub fn section(p: &Persona) -> String {
    format!(
        "\n# Persona (tone only)\n\n\
         The operator picked the persona \"{name}\" for this conversation. It \
         governs TONE ONLY - word choice, rhythm and attitude of what you say. \
         It never changes what you do. Facts, commands, file paths, task ids, \
         how a duplicate warning is handled (never `--force` it yourself), the \
         write policy, and telling the operator the task id `magi task add` \
         prints all stay exact and exactly as described above. Do not let the \
         voice make an answer vaguer, shorter on substance, or more certain \
         than the facts. Never put the persona's voice into the <instruction> \
         you pass to `magi task add`, into files, or into commit messages: \
         those stay neutral.\n\n\
         Voice:\n{voice}\n",
        name = p.name,
        voice = p.prompt,
    )
}

/// What a resumed session is told when the persona changed since the briefing
/// it holds. `None` means the operator went back to the plain voice.
pub fn update_block(p: Option<&Persona>) -> String {
    match p {
        Some(p) => format!(
            "# Persona update\n\nThe operator changed the persona. Drop any earlier \
             persona and use this one from now on.\n{}",
            section(p)
        ),
        None => "# Persona update\n\nThe operator turned the persona off. Drop any earlier \
                 persona and go back to your plain, normal voice from now on.\n"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, name: &str, prompt: &str) -> PersonaSpec {
        PersonaSpec {
            id: id.into(),
            name: name.into(),
            prompt: prompt.into(),
        }
    }

    #[test]
    fn builtins_start_with_a_plain_default() {
        let c = builtin_catalog();
        assert_eq!(c[0].id, DEFAULT_ID);
        assert!(c[0].prompt.is_empty());
        for id in [
            "magi", "rei", "misato", "ritsuko", "shinji", "asuka", "kaworu",
        ] {
            let p = c.iter().find(|p| p.id == id).expect(id);
            assert!(p.prompt.contains("operator's language"), "{id}");
        }
    }

    #[test]
    fn a_user_entry_adds_or_overrides_in_place() {
        let c = catalog(&[
            spec("rei", "Rei", "Be curt."),
            spec("gendo", "Gendo", "Be cold."),
        ]);
        let pos = c.iter().position(|p| p.id == "rei").unwrap();
        assert_eq!(pos, 2);
        assert_eq!(c[pos].prompt, "Be curt.");
        assert_eq!(c.last().unwrap().id, "gendo");
        assert_eq!(c.len(), builtin_catalog().len() + 1);
    }

    #[test]
    fn validation_names_the_problem() {
        let err = |s: Vec<PersonaSpec>| validate(&s).unwrap_err().to_string();
        assert!(err(vec![spec(" ", "n", "p")]).contains("empty `id`"));
        assert!(err(vec![spec("a", "n", "  ")]).contains("empty `prompt`"));
        assert!(err(vec![spec("a", "", "p")]).contains("empty `name`"));
        assert!(err(vec![spec("a", "n", "p"), spec("a", "m", "q")]).contains("more than once"));
        assert!(err(vec![spec("default", "n", "p")]).contains("cannot be redefined"));
        assert!(validate(&[spec("a", "n", "p")]).is_ok());
    }

    #[test]
    fn active_treats_default_blank_and_unknown_as_plain() {
        assert!(active(&[], "").is_none());
        assert!(active(&[], DEFAULT_ID).is_none());
        assert!(active(&[], "nobody").is_none());
        assert_eq!(active(&[], "rei").unwrap().id, "rei");
    }
}
