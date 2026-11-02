//! Skill training, read from each linked character's ESI skill queue.
//!
//! The queue is the one thing about a character that changes on a clock rather
//! than on an event, so it is treated the way industry jobs are: the daemon
//! keeps a durable copy, notifies once when a level's finish time passes, and
//! publishes what is training now so the panel never has to reason about the
//! queue itself.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One level of one skill, as ESI lists it in a character's queue.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SkillQueueEntry {
    pub character_id: u64,
    pub character_name: String,
    pub skill_id: u32,
    /// Resolved from the bundled dataset by the daemon. ESI only ever says
    /// which type ID is training.
    pub skill_name: String,
    /// The level this entry trains to, 1 through 5.
    pub level: u8,
    /// Zero-based order in the queue. Position 0 is what is training now.
    pub position: u32,
    /// Absent, together with `finish_at`, while the queue is paused.
    pub start_at: Option<DateTime<Utc>>,
    pub finish_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub level_start_sp: Option<u32>,
    #[serde(default)]
    pub level_end_sp: Option<u32>,
    #[serde(default)]
    pub training_start_sp: Option<u32>,
}

impl SkillQueueEntry {
    /// Whether this level has finished training. A paused entry never has.
    pub fn is_finished(&self, now: DateTime<Utc>) -> bool {
        self.finish_at.is_some_and(|finish_at| finish_at <= now)
    }

    /// "Caldari Frigate V", which is how the game itself names a level.
    pub fn display_name(&self) -> String {
        format!("{} {}", self.skill_name, level_numeral(self.level))
    }
}

/// What the panel needs to know about one character's queue.
///
/// Derived at publication time rather than stored, because "what is training
/// now" is a function of the clock: the head of the queue finishes and the
/// next entry becomes current without ESI saying a word.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
pub struct SkillQueue {
    /// The entry at the head of the queue, whether or not it is advancing.
    pub training: Option<SkillQueueEntry>,
    /// The two entries after it, so the panel can say what comes next.
    #[serde(default)]
    pub next: Vec<SkillQueueEntry>,
    /// Every unfinished entry, including `training` and `next`.
    pub remaining: u32,
    /// When the last queued level finishes. Absent for an empty or paused
    /// queue.
    pub ends_at: Option<DateTime<Utc>>,
    /// A queue with entries but no dates is paused, which EVE does when the
    /// character's account lapses or the player pauses it deliberately.
    pub paused: bool,
}

impl SkillQueue {
    /// Reduces a full queue to what is still ahead of `now`.
    pub fn summarize(entries: &[SkillQueueEntry], now: DateTime<Utc>) -> Self {
        let mut remaining: Vec<&SkillQueueEntry> = entries
            .iter()
            .filter(|entry| !entry.is_finished(now))
            .collect();
        remaining.sort_by_key(|entry| (entry.position, entry.finish_at));
        let paused = remaining
            .first()
            .is_some_and(|entry| entry.finish_at.is_none());
        Self {
            training: remaining.first().map(|entry| (*entry).clone()),
            next: remaining
                .iter()
                .skip(1)
                .take(2)
                .map(|entry| (*entry).clone())
                .collect(),
            remaining: u32::try_from(remaining.len()).unwrap_or(u32::MAX),
            ends_at: remaining.iter().filter_map(|entry| entry.finish_at).max(),
            paused,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }
}

/// A level that has finished training.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SkillCompletion {
    pub character_id: u64,
    pub character_name: String,
    pub skill_id: u32,
    pub skill_name: String,
    pub level: u8,
    pub finished_at: DateTime<Utc>,
    /// Whether the completion notification has gone out. Kept so a restart
    /// cannot announce the same level twice.
    pub notified: bool,
}

impl SkillCompletion {
    pub fn display_name(&self) -> String {
        format!("{} {}", self.skill_name, level_numeral(self.level))
    }
}

/// Skill levels are written in roman numerals everywhere in EVE.
pub fn level_numeral(level: u8) -> &'static str {
    match level {
        1 => "I",
        2 => "II",
        3 => "III",
        4 => "IV",
        5 => "V",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(skill_id: u32, position: u32, finish_at: Option<&str>) -> SkillQueueEntry {
        SkillQueueEntry {
            character_id: 42,
            character_name: "Example Pilot".to_owned(),
            skill_id,
            skill_name: format!("Skill {skill_id}"),
            level: 4,
            position,
            start_at: finish_at.map(|_| "2026-09-01T00:00:00Z".parse().unwrap()),
            finish_at: finish_at.map(|at| at.parse().unwrap()),
            level_start_sp: None,
            level_end_sp: None,
            training_start_sp: None,
        }
    }

    #[test]
    fn the_summary_skips_finished_levels_and_keeps_queue_order() {
        let now = "2026-09-07T12:00:00Z".parse().unwrap();
        let entries = vec![
            entry(3, 2, Some("2026-09-20T00:00:00Z")),
            entry(1, 0, Some("2026-09-07T11:00:00Z")),
            entry(2, 1, Some("2026-09-10T00:00:00Z")),
            entry(4, 3, Some("2026-09-25T00:00:00Z")),
        ];

        let queue = SkillQueue::summarize(&entries, now);

        assert_eq!(queue.training.as_ref().map(|e| e.skill_id), Some(2));
        assert_eq!(
            queue.next.iter().map(|e| e.skill_id).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(queue.remaining, 3);
        assert_eq!(queue.ends_at, Some("2026-09-25T00:00:00Z".parse().unwrap()));
        assert!(!queue.paused);
    }

    #[test]
    fn a_queue_without_dates_is_paused_not_empty() {
        let now = Utc::now();
        let queue = SkillQueue::summarize(&[entry(1, 0, None), entry(2, 1, None)], now);

        assert!(queue.paused);
        assert_eq!(queue.remaining, 2);
        assert!(queue.ends_at.is_none());
        assert_eq!(queue.training.as_ref().map(|e| e.skill_id), Some(1));
    }

    #[test]
    fn an_empty_queue_says_so() {
        let queue = SkillQueue::summarize(&[], Utc::now());
        assert!(queue.is_empty());
        assert!(queue.training.is_none());
        assert!(!queue.paused);
    }

    #[test]
    fn levels_are_named_the_way_the_game_names_them() {
        let mut level_five = entry(3330, 0, None);
        level_five.skill_name = "Caldari Frigate".to_owned();
        level_five.level = 5;
        assert_eq!(level_five.display_name(), "Caldari Frigate V");
        assert_eq!(level_numeral(0), "?");
    }
}
