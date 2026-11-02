use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const MASS_PRODUCTION: u32 = 3387;
const ADVANCED_MASS_PRODUCTION: u32 = 24_625;
const LABORATORY_OPERATION: u32 = 3406;
const ADVANCED_LABORATORY_OPERATION: u32 = 24_624;

/// How many jobs a character's skills let it run at once.
///
/// One of each to start with, plus one per level of the slot skill and of its
/// advanced version, so eleven at most. Research, copying and invention share
/// the science slots; reactions have their own and are not counted here.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
pub struct SlotCaps {
    pub manufacturing: u8,
    pub science: u8,
}

impl SlotCaps {
    pub fn from_levels(levels: &BTreeMap<u32, u8>) -> Self {
        let cap = |base: u32, advanced: u32| {
            let level = |skill| levels.get(&skill).copied().unwrap_or(0).min(5);
            1 + level(base) + level(advanced)
        };
        Self {
            manufacturing: cap(MASS_PRODUCTION, ADVANCED_MASS_PRODUCTION),
            science: cap(LABORATORY_OPERATION, ADVANCED_LABORATORY_OPERATION),
        }
    }
}

/// Who a job belongs to.
///
/// A job installed by a linked character can also appear in the corporation
/// list. The job ID is the same in both, so ownership decides which listing
/// governs the row: personal wins, because that character is the one whose
/// timers and notifications matter.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOwner {
    #[default]
    Personal,
    Corporation,
}

impl JobOwner {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Corporation => "corporation",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "corporation" => Self::Corporation,
            _ => Self::Personal,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize)]
pub struct IndustryJob {
    pub character_id: u64,
    pub character_name: String,
    pub job_id: u64,
    pub activity_id: u32,
    pub item_type_id: u32,
    pub item_name: String,
    pub runs: u32,
    pub status: String,
    pub start_at: DateTime<Utc>,
    pub end_at: DateTime<Utc>,
    #[serde(default)]
    pub owner: JobOwner,
    #[serde(default)]
    pub corporation_id: Option<u64>,
}

impl IndustryJob {
    pub fn is_running(&self, now: DateTime<Utc>) -> bool {
        self.status == "active" && self.end_at > now
    }

    pub fn is_ready(&self, now: DateTime<Utc>) -> bool {
        self.status == "ready" || (self.status == "active" && self.end_at <= now)
    }

    /// Whether the job holds a manufacturing slot.
    pub fn uses_manufacturing_slot(&self) -> bool {
        self.activity_id == 1
    }

    /// Whether the job holds a science slot: research, copying, reverse
    /// engineering or invention.
    pub fn uses_science_slot(&self) -> bool {
        matches!(self.activity_id, 3 | 4 | 5 | 7 | 8)
    }

    pub fn activity_name(&self) -> &'static str {
        match self.activity_id {
            1 => "Manufacturing",
            3 => "Time efficiency research",
            4 => "Material efficiency research",
            5 => "Copying",
            7 => "Reverse engineering",
            8 => "Invention",
            9 | 11 => "Reaction",
            _ => "Industry",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct IndustryJobView {
    #[serde(flatten)]
    pub job: IndustryJob,
    pub expected_ready: bool,
    pub notified: bool,
    /// Cache-relative path to this job's item icon, when it is on disk.
    #[serde(default)]
    pub icon: Option<String>,
    /// Cache-relative path to the installer's portrait, when it is on disk.
    ///
    /// Whose job it is to collect is the one thing a finished job needs to say
    /// that its own name cannot, and a face answers it faster than a name.
    #[serde(default)]
    pub portrait: Option<String>,
    /// How far through its run the job is, from 0.0 to 1.0.
    ///
    /// Derived rather than stored: it is a pure function of the start and end
    /// times and the current moment, so persisting it would only create a
    /// value that goes stale between writes.
    #[serde(default)]
    pub progress: f64,
}

impl IndustryJob {
    /// Fraction of the run elapsed at `now`, clamped to 0.0..=1.0.
    pub fn progress(&self, now: DateTime<Utc>) -> f64 {
        let total = (self.end_at - self.start_at).num_milliseconds();
        if total <= 0 {
            // A zero-length or inverted window is complete by definition; the
            // alternative is dividing by zero and rendering a NaN bar.
            return 1.0;
        }
        let elapsed = (now - self.start_at).num_milliseconds();
        (elapsed as f64 / total as f64).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(status: &str, end_at: &str) -> IndustryJob {
        IndustryJob {
            character_id: 42,
            character_name: "Example Pilot".to_owned(),
            job_id: 99,
            activity_id: 1,
            item_type_id: 22_456,
            item_name: "Sabre".to_owned(),
            runs: 1,
            status: status.to_owned(),
            start_at: "2026-09-03T08:00:00Z".parse().unwrap(),
            end_at: end_at.parse().unwrap(),
            owner: JobOwner::Personal,
            corporation_id: None,
        }
    }

    #[test]
    fn slot_caps_count_both_skills_on_top_of_the_one_free_slot() {
        let untrained = SlotCaps::from_levels(&BTreeMap::new());
        assert_eq!(
            untrained,
            SlotCaps {
                manufacturing: 1,
                science: 1
            }
        );

        let levels = BTreeMap::from([
            (MASS_PRODUCTION, 5),
            (ADVANCED_MASS_PRODUCTION, 4),
            (LABORATORY_OPERATION, 5),
            (ADVANCED_LABORATORY_OPERATION, 5),
        ]);
        assert_eq!(
            SlotCaps::from_levels(&levels),
            SlotCaps {
                manufacturing: 10,
                science: 11
            }
        );
    }

    #[test]
    fn progress_runs_from_zero_to_one_and_clamps_outside_the_window() {
        let job = job("active", "2026-09-03T10:00:00Z");
        let at = |value: &str| value.parse::<DateTime<Utc>>().unwrap();

        assert_eq!(job.progress(at("2026-09-03T08:00:00Z")), 0.0);
        assert_eq!(job.progress(at("2026-09-03T09:00:00Z")), 0.5);
        assert_eq!(job.progress(at("2026-09-03T10:00:00Z")), 1.0);
        // Before it started and long after it ended both stay in range.
        assert_eq!(job.progress(at("2026-09-01T00:00:00Z")), 0.0);
        assert_eq!(job.progress(at("2027-01-01T00:00:00Z")), 1.0);
    }

    #[test]
    fn a_zero_length_job_is_complete_rather_than_undefined() {
        let job = job("active", "2026-09-03T08:00:00Z");

        assert_eq!(job.progress("2026-09-03T08:00:00Z".parse().unwrap()), 1.0);
    }

    #[test]
    fn job_ownership_round_trips_through_its_stored_form() {
        assert_eq!(JobOwner::parse("corporation"), JobOwner::Corporation);
        assert_eq!(JobOwner::parse("personal"), JobOwner::Personal);
        // An unrecognised value must not silently become a corporation job.
        assert_eq!(JobOwner::parse("nonsense"), JobOwner::Personal);
        assert_eq!(JobOwner::Corporation.as_str(), "corporation");
    }

    #[test]
    fn active_jobs_become_expected_ready_at_their_end_time() {
        let now = "2026-09-03T09:00:00Z".parse().unwrap();

        assert!(job("active", "2026-09-03T09:01:00Z").is_running(now));
        assert!(job("active", "2026-09-03T09:00:00Z").is_ready(now));
        assert!(job("ready", "2026-09-03T10:00:00Z").is_ready(now));
        assert!(!job("paused", "2026-09-03T10:00:00Z").is_running(now));
        assert!(!job("delivered", "2026-09-03T08:30:00Z").is_ready(now));
    }
}
