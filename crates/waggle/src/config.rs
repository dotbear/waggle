//! Waggle's settings file.

use std::{fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use waggle_core::config::{IntelConfig, JabberConfig, LogsConfig, PapsConfig, PrivacyMode};

/// Waggle's public EVE SSO application: Authorization Code with PKCE, no
/// secret. Registered 2026-11-02 with the five read scopes in PLAN.md §6. A
/// client ID is public by design; every copy of Waggle carries it.
pub const EVE_CLIENT_ID: &str = "53f06eddd7454cd4bdd7e8651499199d";

/// Must match the callback registered for [`EVE_CLIENT_ID`] exactly. One port
/// above EVEmarchy's 17632 and away from RIFT's 25252, so all three can run
/// side by side.
pub const EVE_CALLBACK: &str = "http://127.0.0.1:17633/callback";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub logs: LogsConfig,
    pub intel: IntelConfig,
    pub jabber: JabberConfig,
    pub paps: PapsConfig,
    pub notifications: NotificationsConfig,
    pub esi: EsiConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Desktop notifications at all. Everything still shows in the window.
    pub enabled: bool,
    pub privacy: PrivacyMode,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            privacy: PrivacyMode::Off,
        }
    }
}

/// Only for someone who registered an EVE application of their own; the
/// built-in one is what everybody else uses.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EsiConfig {
    pub client_id: String,
    pub redirect_uri: String,
}

impl Default for EsiConfig {
    fn default() -> Self {
        Self {
            client_id: EVE_CLIENT_ID.to_owned(),
            redirect_uri: EVE_CALLBACK.to_owned(),
        }
    }
}

impl Config {
    /// Reads the settings, or the defaults when there are none yet.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let source = fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let config: Self = toml::from_str(&source)
            .with_context(|| format!("could not parse {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.intel.validate()?;
        self.jabber.validate()?;
        self.paps.validate()?;
        Ok(())
    }

    /// Writes the settings through a temporary file, so a crash mid-write
    /// never leaves half a file behind.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("could not serialize the settings")?;
        let temporary = path.with_extension("toml.new");
        fs::write(&temporary, text)
            .with_context(|| format!("could not write {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("could not replace {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_validate() {
        let temp = std::env::temp_dir().join(format!("waggle-config-{}", std::process::id()));
        fs::create_dir_all(&temp).unwrap();
        let path = temp.join("config.toml");
        Config::default().save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.esi.client_id, EVE_CLIENT_ID);
        assert!(loaded.notifications.enabled);
        fs::remove_dir_all(&temp).unwrap();
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<Config>("[notifications]\nenabeld = true\n").is_err());
    }
}
