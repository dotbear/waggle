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

/// The only value `jabber.client_identity` ever meaningfully took. Anything
/// else, such as the retired `"rift"`, is refused when the file is read.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegacyClientIdentity {
    Evemarchy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct JabberConfig {
    pub jid: Option<String>,
    pub directorbot: Option<String>,
    pub freshness_seconds: u64,
    /// The staging system nearly every fleet forms up in.
    ///
    /// Almost every ping names the same system, so repeating it on every row
    /// spends the panel's width on a constant. Naming it here lets the panel
    /// hide the expected case and shout about the exception instead — which is
    /// the only time the formup location is news.
    pub home_formup: Option<String>,
    /// Accepted and ignored. EVEmarchy could once announce itself as RIFT,
    /// and its configuration still says `client_identity = "evemarchy"`;
    /// every front end now always announces its own name.
    #[serde(default, skip_serializing, rename = "client_identity")]
    pub legacy_client_identity: Option<LegacyClientIdentity>,
    /// Broadcast targets whose pings are kept but never announced.
    ///
    /// Directorbot carries more than fleet ops. Opt-in groups use the same bot
    /// for recruitment and newsletter traffic, which is worth having in
    /// history and never worth interrupting for — and a notification that is
    /// reliably not worth reading is what teaches someone to ignore the one
    /// that is.
    ///
    /// A configured list rather than a constant because the group names belong
    /// to one alliance, and this repository does not carry them.
    pub muted_targets: Vec<String>,
}

impl JabberConfig {
    /// Whether a broadcast aimed at this target is recorded silently.
    ///
    /// Matched on the target named in Directorbot's footer, case-insensitively
    /// and ignoring surrounding space, because that is a slug typed into a
    /// configuration file by hand.
    pub fn target_is_muted(&self, target: Option<&str>) -> bool {
        let Some(target) = target.map(str::trim).filter(|it| !it.is_empty()) else {
            return false;
        };
        self.muted_targets
            .iter()
            .any(|muted| muted.trim().eq_ignore_ascii_case(target))
    }
}

impl Default for JabberConfig {
    fn default() -> Self {
        Self {
            jid: None,
            directorbot: None,
            freshness_seconds: 300,
            home_formup: None,
            legacy_client_identity: None,
            muted_targets: Vec::new(),
        }
    }
}

impl JabberConfig {
    pub fn validate(&self) -> Result<()> {
        if let Some(jid) = &self.jid
            && (!jid.contains('@') || jid.trim() != jid)
        {
            bail!("jabber.jid must be a bare JID such as name@example.com");
        }

        if let Some(directorbot) = &self.directorbot
            && (!directorbot.contains('@') || directorbot.trim() != directorbot)
        {
            bail!("jabber.directorbot must be a bare JID");
        }

        if self.freshness_seconds == 0 {
            bail!("jabber.freshness_seconds must be greater than zero");
        }

        if self
            .home_formup
            .as_ref()
            .is_some_and(|system| system.trim().is_empty() || system.trim() != system)
        {
            bail!("jabber.home_formup must not be blank or contain surrounding whitespace");
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

    #[test]
    fn the_retired_rift_identity_is_refused_and_evemarchys_is_ignored() {
        let accepted: JabberConfig = toml::from_str("client_identity = \"evemarchy\"\n").unwrap();
        accepted.validate().unwrap();
        // Never written back out: it says nothing any more.
        assert!(
            !toml::to_string(&accepted)
                .unwrap()
                .contains("client_identity")
        );

        let error = toml::from_str::<JabberConfig>("client_identity = \"rift\"\n").unwrap_err();
        assert!(error.to_string().contains("rift"), "{error}");
    }

    #[test]
    fn jabber_rejects_malformed_addresses() {
        for broken in [
            JabberConfig {
                jid: Some("no-at-sign".to_owned()),
                ..JabberConfig::default()
            },
            JabberConfig {
                directorbot: Some(" bot@example.invalid".to_owned()),
                ..JabberConfig::default()
            },
            JabberConfig {
                freshness_seconds: 0,
                ..JabberConfig::default()
            },
            JabberConfig {
                home_formup: Some(" ".to_owned()),
                ..JabberConfig::default()
            },
        ] {
            assert!(broken.validate().is_err(), "{broken:?} should not validate");
        }
        JabberConfig::default().validate().unwrap();
    }
}
