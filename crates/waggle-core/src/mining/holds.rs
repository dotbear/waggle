//! How much ore a ship can carry before its lasers stop.
//!
//! The base size and the bonuses come from CCP's SDE; the only thing that
//! grows a mining hold in game is the hull's own skill bonus (no module, rig
//! or implant touches the attribute), so a ship type and the pilot's skill
//! levels are the whole answer.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::LazyLock};

const DATA: &str = include_str!("../../assets/mining-holds-v1.tsv");

struct Hull {
    base_micro: i64,
    /// `(skill, percent per level)`; skill 0 is a flat role bonus.
    bonuses: Vec<(u32, f64)>,
}

static HULLS: LazyLock<BTreeMap<u32, Hull>> = LazyLock::new(|| {
    DATA.lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next().and_then(|v| v.parse().ok());
            let base_micro = fields.next().and_then(|v| v.parse().ok());
            let bonuses = fields
                .next()
                .filter(|v| *v != "-")
                .map(|v| {
                    v.split(';')
                        .map(|pair| {
                            let (skill, percent) = pair.split_once(':').expect("skill:percent");
                            (
                                skill.parse().expect("skill id"),
                                percent.parse().expect("percent"),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                id.expect("ship type id"),
                Hull {
                    base_micro: base_micro.expect("base hold"),
                    bonuses,
                },
            )
        })
        .collect()
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MiningHold {
    pub base_m3: f64,
    pub capacity_m3: f64,
    /// False when the hull has a skill bonus and the pilot's skills could not
    /// be read, in which case `capacity_m3` is the unskilled base and the real
    /// hold is larger.
    pub skills_known: bool,
}

/// The mining hold of `ship_type_id` flown with `skills` (skill → active
/// level), or `None` for a ship that has no mining hold.
pub fn mining_hold(ship_type_id: u32, skills: Option<&BTreeMap<u32, u8>>) -> Option<MiningHold> {
    let hull = HULLS.get(&ship_type_id)?;
    let base_m3 = hull.base_micro as f64 / 1_000_000.0;
    let mut capacity_m3 = base_m3;
    let mut skills_known = true;
    for &(skill, percent) in &hull.bonuses {
        let level = if skill == 0 {
            1
        } else if let Some(skills) = skills {
            skills.get(&skill).copied().unwrap_or(0)
        } else {
            skills_known = false;
            0
        };
        capacity_m3 *= 1.0 + percent * f64::from(level) / 100.0;
    }
    Some(MiningHold {
        base_m3,
        capacity_m3,
        skills_known,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETRIEVER: u32 = 17478;
    const MACKINAW: u32 = 22548;
    const PROCURER: u32 = 17480;
    const MINING_BARGE: u32 = 17940;
    const EXHUMERS: u32 = 22551;

    #[test]
    fn a_hull_with_no_bonus_is_its_base_size_whatever_the_skills() {
        let hold = mining_hold(PROCURER, None).unwrap();
        assert_eq!(hold.capacity_m3, 16_000.0);
        assert!(hold.skills_known);
    }

    #[test]
    fn each_skill_bonus_multiplies_by_level() {
        let skills = BTreeMap::from([(MINING_BARGE, 5), (EXHUMERS, 4)]);
        let hold = mining_hold(MACKINAW, Some(&skills)).unwrap();
        // 31,500 × 1.25 × 1.10
        assert!((hold.capacity_m3 - 43_312.5).abs() < 1e-6);
        let hold = mining_hold(RETRIEVER, Some(&skills)).unwrap();
        assert!((hold.capacity_m3 - 34_375.0).abs() < 1e-6);
    }

    #[test]
    fn unread_skills_give_the_base_and_say_so() {
        let hold = mining_hold(RETRIEVER, None).unwrap();
        assert_eq!(hold.capacity_m3, 27_500.0);
        assert!(!hold.skills_known);
    }

    #[test]
    fn a_ship_without_a_mining_hold_has_none() {
        // Rifter.
        assert_eq!(mining_hold(587, None), None);
    }
}
