use std::collections::HashSet;

use serde::Serialize;

use crate::universe::{ShipType, SolarSystem, SystemResolution, Universe};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    Report,
    Clear,
    Question,
    Movement,
    Unknown,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct ResolvedSystem {
    pub system: SolarSystem,
    pub matched: String,
    pub exact: bool,
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct Understanding {
    pub classification: Classification,
    pub systems: Vec<ResolvedSystem>,
    pub ships: Vec<ShipType>,
    pub has_character_signal: bool,
    pub evidence: Vec<String>,
}

impl Understanding {
    pub fn matches_any_character_or_ship(&self) -> bool {
        self.classification == Classification::Report
            && (self.has_character_signal || !self.ships.is_empty())
    }
}

const HOSTILE_MARKERS: [&str; 9] = [
    "neut", "neuts", "neutral", "neutrals", "hostile", "hostiles", "red", "reds", "spike",
];

const CLEAR_PHRASES: [&str; 6] = [
    "clear",
    "clr",
    "nv",
    "no visual",
    "nothing seen",
    "all clear",
];

const MOVEMENT_MARKERS: [&str; 5] = ["jumping", "jumped", "towards", "toward", "moving"];

/// Hull-class shorthand used in intel channels.
///
/// These name a class rather than one hull, so they cannot resolve to a type ID
/// the way "Sabre" does. They are treated as entity evidence instead: the point
/// of the alert is that something hostile was reported, and the class is enough
/// to know that. Hand-written from common usage, not imported from any other
/// tool's data.
const SHIP_SLANG: [&str; 30] = [
    "bomber", "bombers", "bubbler", "bubblers", "cap", "caps", "capital", "capitals", "carrier",
    "carriers", "cepter", "cepters", "ceptor", "ceptors", "dictor", "dictors", "dread", "dreads",
    "fax", "faxes", "hic", "hics", "hictor", "hictors", "logi", "marauder", "recon", "super",
    "supers", "titan",
];

/// Words that carry no entity information on their own. Kept out of the
/// character-name heuristic so ordinary channel chatter does not read as intel.
const FILLER_WORDS: [&str; 33] = [
    "a", "an", "and", "any", "are", "at", "back", "confirm", "eyes", "for", "from", "gate",
    "gates", "holding", "in", "is", "it", "just", "not", "now", "of", "on", "or", "out",
    "reported", "seen", "status", "the", "there", "to", "up", "visual", "with",
];

pub struct Parser<'a> {
    universe: &'a Universe,
}

impl<'a> Parser<'a> {
    pub fn new(universe: &'a Universe) -> Self {
        Self { universe }
    }

    pub fn parse(&self, message: &str, region_hints: &[String]) -> Understanding {
        let normalized = normalize(message);
        let words = message.split_whitespace().collect::<Vec<_>>();
        let mut seen_systems = HashSet::new();
        let systems = words
            .iter()
            .filter_map(|word| {
                let resolution = self.universe.resolve_system(word, region_hints)?;
                let system = resolution.system().clone();
                seen_systems.insert(system.id).then(|| ResolvedSystem {
                    system,
                    matched: (*word).to_owned(),
                    exact: matches!(resolution, SystemResolution::Exact(_)),
                })
            })
            .collect::<Vec<_>>();
        let ships = self.universe.ships_in_text(message);

        let is_question = message.contains('?')
            || contains_phrase(&normalized, "status")
            || contains_phrase(&normalized, "status pls")
            || contains_phrase(&normalized, "any eyes");
        let is_clear = CLEAR_PHRASES
            .iter()
            .any(|phrase| contains_phrase(&normalized, phrase));
        let has_hostile_marker = HOSTILE_MARKERS
            .iter()
            .any(|marker| contains_phrase(&normalized, marker));
        let has_ship_slang = SHIP_SLANG
            .iter()
            .any(|marker| contains_phrase(&normalized, marker));
        let has_movement = MOVEMENT_MARKERS
            .iter()
            .any(|marker| contains_phrase(&normalized, marker))
            || message.contains("->")
            || message.contains("<-");
        let has_character_like_text = !systems.is_empty()
            && ships.is_empty()
            && contains_character_like_text(&words, &systems);
        let has_character_signal = has_hostile_marker || has_ship_slang || has_character_like_text;

        // A named ship or an explicit hostile word is positive evidence. A bare
        // question mark alongside it usually means "hostiles are here, is anyone
        // else seeing this?", so a question must never suppress that. A clear
        // phrase does outrank a bare hostile word ("q4c clear of reds") but not
        // a named ship, because clear reports do not name ship types.
        let has_entity_evidence = !ships.is_empty() || has_hostile_marker || has_ship_slang;

        let classification = if systems.is_empty() {
            Classification::Unknown
        } else if is_clear && ships.is_empty() {
            Classification::Clear
        } else if has_entity_evidence {
            Classification::Report
        } else if is_question {
            Classification::Question
        } else if has_character_signal {
            Classification::Report
        } else if has_movement {
            Classification::Movement
        } else {
            Classification::Unknown
        };

        let mut evidence = Vec::new();
        if !systems.is_empty() {
            evidence.push("system".to_owned());
        }
        if !ships.is_empty() {
            evidence.push("ship".to_owned());
        }
        if has_hostile_marker {
            evidence.push("hostile_keyword".to_owned());
        }
        if has_ship_slang {
            evidence.push("ship_class_slang".to_owned());
        }
        if has_character_like_text {
            evidence.push("unidentified_character".to_owned());
        }
        if is_clear {
            evidence.push("clear_keyword".to_owned());
        }
        if is_question {
            evidence.push("question".to_owned());
        }
        if has_movement {
            evidence.push("movement".to_owned());
        }

        Understanding {
            classification,
            systems,
            ships,
            has_character_signal,
            evidence,
        }
    }
}

fn normalize(message: &str) -> String {
    message
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '-' {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_phrase(normalized: &str, phrase: &str) -> bool {
    format!(" {normalized} ").contains(&format!(" {phrase} "))
}

/// The configured rule is "any character or any ship", and version 1 does not
/// resolve character names through ESI. So any remaining word that is not a
/// system, a filler word, or an already-counted hostile word is treated as a
/// possible pilot name. This is deliberately generous: it means the effective
/// rule is closer to "any unexplained text next to a system name". The filler
/// list exists to keep routine channel chatter from qualifying.
fn contains_character_like_text(words: &[&str], systems: &[ResolvedSystem]) -> bool {
    let system_tokens = systems
        .iter()
        .map(|system| normalize(&system.matched))
        .collect::<HashSet<_>>();

    words.iter().any(|word| {
        let token = normalize(word);
        token.len() >= 2
            && token.chars().any(char::is_alphabetic)
            && !system_tokens.contains(&token)
            && !FILLER_WORDS.contains(&token.as_str())
            && !HOSTILE_MARKERS.contains(&token.as_str())
            && !CLEAR_PHRASES.contains(&token.as_str())
            && !MOVEMENT_MARKERS.contains(&token.as_str())
            && !SHIP_SLANG.contains(&token.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn universe() -> Universe {
        Universe::from_tsv(
            "B\t123\n\
             R\t10\tExample Region\n\
             R\t11\tOther Region\n\
             S\t1\t10\tQ4C-S5\n\
             S\t2\t10\t4-CM8I\n\
             S\t3\t11\tQ4X-XX\n\
             G\t1\t2\n\
             T\t100\tSabre\n\
             T\t101\tFederation Navy Comet\n",
        )
        .unwrap()
    }

    #[test]
    fn classifies_a_known_ship_report() {
        let universe = universe();
        let parsed = Parser::new(&universe).parse("Q4C-S5 Sabre on gate", &[]);

        assert_eq!(parsed.classification, Classification::Report);
        assert_eq!(parsed.systems[0].system.name, "Q4C-S5");
        assert_eq!(parsed.ships[0].name, "Sabre");
        assert!(!parsed.has_character_signal);
        assert!(parsed.matches_any_character_or_ship());
    }

    #[test]
    fn classifies_an_unspecified_hostile_report() {
        let universe = universe();
        let parsed = Parser::new(&universe).parse("2 neuts in q4c", &[]);

        assert_eq!(parsed.classification, Classification::Report);
        assert!(parsed.has_character_signal);
        assert!(parsed.evidence.contains(&"hostile_keyword".to_owned()));
    }

    #[test]
    fn treats_free_text_as_an_unidentified_character_candidate() {
        let universe = universe();
        let parsed = Parser::new(&universe).parse("Example Pilot q4c", &[]);

        assert_eq!(parsed.classification, Classification::Report);
        assert!(parsed.has_character_signal);
        assert!(
            parsed
                .evidence
                .contains(&"unidentified_character".to_owned())
        );
    }

    #[test]
    fn clear_and_question_messages_do_not_match_the_alert() {
        let universe = universe();
        let parser = Parser::new(&universe);

        let clear = parser.parse("q4c clear", &[]);
        assert_eq!(clear.classification, Classification::Clear);
        assert!(!clear.matches_any_character_or_ship());

        let question = parser.parse("any eyes in q4c?", &[]);
        assert_eq!(question.classification, Classification::Question);
        assert!(!question.matches_any_character_or_ship());
    }

    #[test]
    fn a_question_mark_does_not_suppress_a_hostile_report() {
        let universe = universe();
        let parser = Parser::new(&universe);

        let uncertain = parser.parse("3 neuts q4c-s5?? anyone confirm", &[]);
        assert_eq!(uncertain.classification, Classification::Report);
        assert!(uncertain.matches_any_character_or_ship());

        let with_ship = parser.parse("Q4C-S5 Sabre on gate?", &[]);
        assert_eq!(with_ship.classification, Classification::Report);
        assert!(with_ship.matches_any_character_or_ship());

        let status_request = parser.parse("q4c-s5 hostile status", &[]);
        assert_eq!(status_request.classification, Classification::Report);
    }

    #[test]
    fn hull_class_shorthand_counts_as_entity_evidence() {
        let universe = universe();
        let parser = Parser::new(&universe);

        for message in [
            "dictor on gate q4c-s5",
            "2 bombers q4c-s5",
            "q4c-s5 hictor tackling",
            "caps spotted q4c-s5?",
        ] {
            let parsed = parser.parse(message, &[]);
            assert_eq!(
                parsed.classification,
                Classification::Report,
                "{message} should report"
            );
            assert!(parsed.matches_any_character_or_ship(), "{message}");
            assert!(
                parsed.evidence.contains(&"ship_class_slang".to_owned()),
                "{message}"
            );
        }

        // Slang is still outranked by a clear, exactly like a hostile word.
        assert_eq!(
            parser.parse("q4c-s5 clear, no dictors", &[]).classification,
            Classification::Clear
        );
    }

    #[test]
    fn a_clear_message_still_reports_a_named_ship() {
        let universe = universe();
        let parser = Parser::new(&universe);

        let cleared = parser.parse("q4c-s5 clear of reds", &[]);
        assert_eq!(cleared.classification, Classification::Clear);
        assert!(!cleared.matches_any_character_or_ship());

        // A clear phrase about one system must not hide a ship report.
        let mixed = parser.parse("4-CM8I clear, Sabre in Q4C-S5", &[]);
        assert_eq!(mixed.classification, Classification::Report);
        assert!(mixed.matches_any_character_or_ship());
    }

    #[test]
    fn routine_chatter_without_an_entity_is_not_a_report() {
        let universe = universe();
        let parser = Parser::new(&universe);

        assert_eq!(
            parser.parse("q4c-s5 gate is up", &[]).classification,
            Classification::Unknown
        );
        assert_eq!(
            parser.parse("any eyes in q4c-s5", &[]).classification,
            Classification::Question
        );
    }

    #[test]
    fn movement_without_an_entity_is_not_a_report() {
        let universe = universe();
        let parsed = Parser::new(&universe).parse("jumping towards q4c", &[]);

        assert_eq!(parsed.classification, Classification::Movement);
        assert!(!parsed.matches_any_character_or_ship());
    }

    #[test]
    fn region_hints_disambiguate_short_system_prefixes() {
        let universe = universe();
        let parsed = Parser::new(&universe).parse("q4 hostile", &["Example Region".to_owned()]);

        assert_eq!(parsed.classification, Classification::Report);
        assert_eq!(parsed.systems[0].system.name, "Q4C-S5");
        assert!(!parsed.systems[0].exact);
    }
}
