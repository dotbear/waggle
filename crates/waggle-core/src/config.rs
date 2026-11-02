//! Configuration sections shared by every Waggle front end.
//!
//! Each section validates itself, so an application can assemble its own
//! top-level configuration from these and add sections of its own without
//! losing `deny_unknown_fields` (which serde cannot combine with `flatten`).

use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogsConfig {
    pub roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntelConfig {
    pub freshness_seconds: u64,
    pub presence_grace_seconds: u64,
    pub cooldown_seconds: u64,
    pub max_jumps: u8,
    pub only_undocked: bool,
    /// Count alliance jump bridges as hops when measuring range.
    ///
    /// On by default because that is how a nullsec pilot thinks about their own
    /// space. Note that hostiles usually cannot use your bridges, so an alert
    /// that was only in range through one is marked as such rather than being
    /// presented as an equally immediate threat.
    pub use_jump_bridges: bool,
    pub channels: BTreeMap<String, Vec<String>>,
}

impl Default for IntelConfig {
    fn default() -> Self {
        Self {
            freshness_seconds: 120,
            presence_grace_seconds: 90,
            cooldown_seconds: 30,
            max_jumps: 2,
            only_undocked: true,
            use_jump_bridges: true,
            channels: BTreeMap::new(),
        }
    }
}

impl IntelConfig {
    pub fn validate(&self) -> Result<()> {
        if self.freshness_seconds == 0 {
            bail!("intel.freshness_seconds must be greater than zero");
        }
        if self.presence_grace_seconds == 0 || self.presence_grace_seconds > 600 {
            bail!("intel.presence_grace_seconds must be between 1 and 600");
        }
        if self.cooldown_seconds == 0 {
            bail!("intel.cooldown_seconds must be greater than zero");
        }
        if self.max_jumps > 20 {
            bail!("intel.max_jumps must not exceed 20");
        }

        for (channel, regions) in &self.channels {
            if channel.trim().is_empty() {
                bail!("intel channel names must not be blank");
            }
            if regions.iter().any(|region| region.trim().is_empty()) {
                bail!("regions for intel channel {channel:?} must not be blank");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PapsConfig {
    /// Request on startup and periodically. Disable for a quiet period while
    /// retaining Jabber pings and explicit `paps refresh` requests.
    pub auto_refresh: bool,
    pub refresh_minutes: u64,
}

impl Default for PapsConfig {
    fn default() -> Self {
        Self {
            auto_refresh: true,
            refresh_minutes: 60,
        }
    }
}

impl PapsConfig {
    pub fn validate(&self) -> Result<()> {
        if self.refresh_minutes == 0 {
            bail!("paps.refresh_minutes must be greater than zero");
        }
        Ok(())
    }
}

/// When notification bodies are withheld.
///
/// Intel and fleet pings quote alliance communications, which a notification
/// daemon will happily render on a lock screen in front of anyone walking past.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrivacyMode {
    /// Always show the full message.
    #[default]
    Off,
    /// Withhold the body only while the screen is locked.
    Locked,
    /// Never include message text in a notification.
    Always,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_valid() {
        IntelConfig::default().validate().unwrap();
        PapsConfig::default().validate().unwrap();
    }

    #[test]
    fn intel_rejects_impossible_timings() {
        for broken in [
            IntelConfig {
                freshness_seconds: 0,
                ..IntelConfig::default()
            },
            IntelConfig {
                presence_grace_seconds: 0,
                ..IntelConfig::default()
            },
            IntelConfig {
                presence_grace_seconds: 601,
                ..IntelConfig::default()
            },
            IntelConfig {
                cooldown_seconds: 0,
                ..IntelConfig::default()
            },
            IntelConfig {
                max_jumps: 21,
                ..IntelConfig::default()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?} should not validate");
        }
    }

    #[test]
    fn intel_rejects_blank_channels_and_regions() {
        let mut config = IntelConfig::default();
        config.channels.insert("  ".to_owned(), vec![]);
        assert!(config.validate().is_err());

        let mut config = IntelConfig::default();
        config
            .channels
            .insert("intel.example".to_owned(), vec!["".to_owned()]);
        assert!(config.validate().is_err());
    }

    #[test]
    fn paps_need_a_refresh_interval() {
        let config = PapsConfig {
            refresh_minutes: 0,
            ..PapsConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn sections_parse_from_toml_and_reject_unknown_keys() {
        let intel: IntelConfig = toml::from_str("max_jumps = 5\n").unwrap();
        assert_eq!(intel.max_jumps, 5);
        assert_eq!(
            intel.cooldown_seconds,
            IntelConfig::default().cooldown_seconds
        );
        assert!(toml::from_str::<IntelConfig>("max_jump = 5\n").is_err());
    }
}
