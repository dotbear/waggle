use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use sha2::{Digest, Sha256};

use crate::{
    config::IntelConfig,
    esi::CharacterPresence,
    intel::Understanding,
    logs::ChatEvent,
    store::NewIntelAlert,
    universe::{SolarSystem, Universe},
};

const STALE_AFTER_SECONDS: i64 = 20;
const MAX_FUTURE_SKEW_SECONDS: i64 = 30;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Suppression {
    FutureMessage,
    StaleMessage,
    NotAReport,
    Cooldown,
    NoEligibleCharacter,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertMatch {
    pub system: SolarSystem,
    pub distance: u16,
    pub character: CharacterPresence,
    pub stale_presence: bool,
    /// True when a jump bridge, not stargates alone, put the report in range.
    pub via_bridge: bool,
}

/// One cooldown slot per reported system and threatened character.
///
/// A single global cooldown would let a distant report silence a closer one, so
/// proximity is evaluated first and the cooldown only suppresses a repeat of the
/// same system/character pair.
#[derive(Debug, Default)]
pub struct AlertPolicy {
    last_alert_at: HashMap<(u32, u64), DateTime<Utc>>,
}

impl AlertPolicy {
    pub fn new() -> Self {
        Self {
            last_alert_at: HashMap::new(),
        }
    }

    pub fn evaluate(
        &self,
        now: DateTime<Utc>,
        occurred_at: DateTime<Utc>,
        understanding: &Understanding,
        characters: &[CharacterPresence],
        universe: &Universe,
        config: &IntelConfig,
    ) -> Result<AlertMatch, Suppression> {
        let age = now.signed_duration_since(occurred_at).num_seconds();
        if age < -MAX_FUTURE_SKEW_SECONDS {
            return Err(Suppression::FutureMessage);
        }
        if age > config.freshness_seconds as i64 {
            return Err(Suppression::StaleMessage);
        }
        if !understanding.matches_any_character_or_ship() || understanding.systems.is_empty() {
            return Err(Suppression::NotAReport);
        }

        let mut cooled_down = false;
        let matched = understanding
            .systems
            .iter()
            .flat_map(|reported| {
                characters.iter().filter_map(move |character| {
                    if character.online != Some(true) {
                        return None;
                    }
                    if config.only_undocked && character.docked != Some(false) {
                        return None;
                    }
                    let observed_at = character.observed_at?;
                    let presence_age = now.signed_duration_since(observed_at).num_seconds().max(0);
                    if presence_age > config.presence_grace_seconds as i64 {
                        return None;
                    }
                    let origin = character.solar_system_id?;
                    let route = universe.route(
                        origin,
                        reported.system.id,
                        u16::from(config.max_jumps),
                        config.use_jump_bridges,
                    )?;
                    Some(AlertMatch {
                        system: reported.system.clone(),
                        distance: route.jumps,
                        character: character.clone(),
                        stale_presence: character.error.is_some()
                            || presence_age > STALE_AFTER_SECONDS,
                        via_bridge: route.via_bridge,
                    })
                })
            })
            .filter(|matched| {
                let key = (matched.system.id, matched.character.character_id);
                let within_cooldown = self.last_alert_at.get(&key).is_some_and(|last| {
                    now.signed_duration_since(*last).num_seconds() < config.cooldown_seconds as i64
                });
                if within_cooldown {
                    cooled_down = true;
                }
                !within_cooldown
            })
            .min_by_key(|matched| (matched.distance, matched.via_bridge, matched.stale_presence));

        match matched {
            Some(matched) => Ok(matched),
            None if cooled_down => Err(Suppression::Cooldown),
            None => Err(Suppression::NoEligibleCharacter),
        }
    }

    pub fn acknowledge(&mut self, at: DateTime<Utc>, matched: &AlertMatch) {
        let cutoff = at - TimeDelta::hours(1);
        self.last_alert_at.retain(|_, last| *last >= cutoff);
        self.last_alert_at
            .insert((matched.system.id, matched.character.character_id), at);
    }
}

pub fn persisted_alert(event: &ChatEvent, matched: &AlertMatch) -> NewIntelAlert {
    NewIntelAlert {
        fingerprint: event_fingerprint(event),
        occurred_at: event.message.timestamp,
        channel: event.channel.clone(),
        author: event.message.author.clone(),
        message: event.message.text.clone(),
        system_id: matched.system.id,
        system_name: matched.system.name.clone(),
        distance: matched.distance,
        character_id: matched.character.character_id,
        character_name: matched.character.character_name.clone(),
        stale_presence: matched.stale_presence,
        via_bridge: matched.via_bridge,
    }
}

fn event_fingerprint(event: &ChatEvent) -> String {
    let mut digest = Sha256::new();
    for value in [
        event.channel.as_bytes(),
        event.message.timestamp.to_rfc3339().as_bytes(),
        event.message.author.as_bytes(),
        event.message.text.as_bytes(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intel::Parser;

    fn universe() -> Universe {
        Universe::from_tsv(
            "B\t123\n\
             R\t10\tExample Region\n\
             S\t1\t10\tHOME-1\n\
             S\t2\t10\tNEAR-2\n\
             S\t3\t10\tFAR-3\n\
             G\t1\t2\n\
             G\t2\t3\n\
             T\t100\tSabre\n",
        )
        .unwrap()
    }

    fn presence(now: DateTime<Utc>) -> CharacterPresence {
        CharacterPresence {
            character_id: 42,
            character_name: "Example Pilot".to_owned(),
            online: Some(true),
            solar_system_id: Some(1),
            docked: Some(false),
            solar_system_name: Some("HOME-1".to_owned()),
            region_name: Some("Example Region".to_owned()),
            portrait: None,
            observed_at: Some(now),
            error: None,
            skill_queue: None,
            skill_queue_error: None,
            ship: None,
            ship_error: None,
            industry_slots: None,
        }
    }

    #[test]
    fn accepts_fresh_report_near_an_online_undocked_character() {
        let now = "2026-09-03T09:00:00Z".parse().unwrap();
        let universe = universe();
        let understanding = Parser::new(&universe).parse("NEAR-2 Sabre", &[]);

        let matched = AlertPolicy::new()
            .evaluate(
                now,
                now - chrono::Duration::seconds(5),
                &understanding,
                &[presence(now)],
                &universe,
                &IntelConfig::default(),
            )
            .unwrap();

        assert_eq!(matched.distance, 1);
        assert_eq!(matched.system.name, "NEAR-2");
        assert!(!matched.stale_presence);
    }

    #[test]
    fn suppresses_docked_offline_stale_and_distant_matches() {
        let now = "2026-09-03T09:00:00Z".parse().unwrap();
        let universe = universe();
        let policy = AlertPolicy::new();
        let near = Parser::new(&universe).parse("NEAR-2 Sabre", &[]);
        let far = Parser::new(&universe).parse("FAR-3 Sabre", &[]);
        let config = IntelConfig {
            max_jumps: 1,
            ..IntelConfig::default()
        };

        let mut docked = presence(now);
        docked.docked = Some(true);
        assert_eq!(
            policy.evaluate(now, now, &near, &[docked], &universe, &config),
            Err(Suppression::NoEligibleCharacter)
        );

        let mut offline = presence(now);
        offline.online = Some(false);
        assert_eq!(
            policy.evaluate(now, now, &near, &[offline], &universe, &config),
            Err(Suppression::NoEligibleCharacter)
        );

        assert_eq!(
            policy.evaluate(
                now,
                now - chrono::Duration::seconds(121),
                &near,
                &[presence(now)],
                &universe,
                &config,
            ),
            Err(Suppression::StaleMessage)
        );
        assert_eq!(
            policy.evaluate(now, now, &far, &[presence(now)], &universe, &config),
            Err(Suppression::NoEligibleCharacter)
        );
    }

    #[test]
    fn applies_the_cooldown_to_the_same_system_and_character() {
        let now = "2026-09-03T09:00:00Z".parse().unwrap();
        let universe = universe();
        let understanding = Parser::new(&universe).parse("NEAR-2 Sabre", &[]);
        let mut policy = AlertPolicy::new();
        let first = policy
            .evaluate(
                now,
                now,
                &understanding,
                &[presence(now)],
                &universe,
                &IntelConfig::default(),
            )
            .unwrap();
        policy.acknowledge(now, &first);

        assert_eq!(
            policy.evaluate(
                now + chrono::Duration::seconds(29),
                now,
                &understanding,
                &[presence(now)],
                &universe,
                &IntelConfig::default(),
            ),
            Err(Suppression::Cooldown)
        );
        assert!(
            policy
                .evaluate(
                    now + chrono::Duration::seconds(30),
                    now,
                    &understanding,
                    &[presence(now)],
                    &universe,
                    &IntelConfig::default(),
                )
                .is_ok()
        );
    }

    #[test]
    fn a_cooled_down_system_never_silences_a_closer_report() {
        let now = "2026-09-03T09:00:00Z".parse().unwrap();
        let universe = universe();
        let parser = Parser::new(&universe);
        let mut policy = AlertPolicy::new();
        let config = IntelConfig::default();

        let far = parser.parse("NEAR-2 Sabre", &[]);
        let acknowledged = policy
            .evaluate(now, now, &far, &[presence(now)], &universe, &config)
            .unwrap();
        policy.acknowledge(now, &acknowledged);

        // Hostiles in the character's own system must still alert immediately.
        let home = parser.parse("HOME-1 Sabre", &[]);
        let matched = policy
            .evaluate(
                now + chrono::Duration::seconds(1),
                now,
                &home,
                &[presence(now)],
                &universe,
                &config,
            )
            .unwrap();

        assert_eq!(matched.distance, 0);
        assert_eq!(matched.system.name, "HOME-1");
    }

    #[test]
    fn the_cooldown_is_scoped_per_character() {
        let now = "2026-09-03T09:00:00Z".parse().unwrap();
        let universe = universe();
        let understanding = Parser::new(&universe).parse("NEAR-2 Sabre", &[]);
        let mut policy = AlertPolicy::new();
        let config = IntelConfig::default();

        let mut second_character = presence(now);
        second_character.character_id = 43;
        second_character.character_name = "Other Pilot".to_owned();

        let first = policy
            .evaluate(
                now,
                now,
                &understanding,
                &[presence(now)],
                &universe,
                &config,
            )
            .unwrap();
        policy.acknowledge(now, &first);

        let matched = policy
            .evaluate(
                now + chrono::Duration::seconds(1),
                now,
                &understanding,
                &[second_character],
                &universe,
                &config,
            )
            .unwrap();

        assert_eq!(matched.character.character_id, 43);
    }
}
