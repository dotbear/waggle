//! The operator's jump-bridge network.
//!
//! Bridges are alliance infrastructure, not facts about EVE, so they are not
//! part of the generated CCP dataset and they do not belong in the hand-edited
//! `config.toml` either: there are typically a hundred of them and they are
//! regenerated wholesale. They live in their own file, which a generator owns
//! and a human can still read.

use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One bridge, as a pair of system names.
///
/// Names rather than IDs so the file stays reviewable, and because whoever
/// maintains the network thinks in system names. Resolution happens once at
/// startup, where an unknown name becomes a warning instead of a silent hole.
pub type Link = [String; 2];

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct JumpBridges {
    /// Where the list came from, for the reader's benefit.
    pub source: Option<String>,
    pub generated_at: Option<DateTime<Utc>>,
    pub links: Vec<Link>,
}

impl JumpBridges {
    /// Loads the network, treating a missing file as an empty one.
    ///
    /// Most installations have no bridges, so absence is normal rather than an
    /// error. A file that exists but does not parse *is* an error: silently
    /// running with no bridges would turn every bridge-adjacent hostile into a
    /// missed alert, which is exactly the failure this data exists to prevent.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let source = fs::read_to_string(path)
            .with_context(|| format!("could not read jump bridges at {}", path.display()))?;
        let bridges: Self = toml::from_str(&source)
            .with_context(|| format!("could not parse jump bridges at {}", path.display()))?;
        bridges.validate()?;
        Ok(bridges)
    }

    pub fn validate(&self) -> Result<()> {
        for link in &self.links {
            if link.iter().any(|name| name.trim().is_empty()) {
                bail!("jump bridge links must name two systems");
            }
        }
        Ok(())
    }

    /// Writes the network, creating the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "could not create configuration directory {}",
                    parent.display()
                )
            })?;
        }
        let mut document = String::from(
            "# EVEmarchy jump-bridge network.\n\
             #\n\
             # Regenerate with `evemarchy import rift`, or edit by hand: each link is a\n\
             # pair of system names and is usable in both directions.\n\n",
        );
        document.push_str(
            &toml::to_string_pretty(self).context("could not serialize the jump-bridge network")?,
        );
        fs::write(path, document)
            .with_context(|| format!("could not write jump bridges to {}", path.display()))?;
        Ok(())
    }

    /// Deduplicates links, ignoring direction, and sorts them for a stable file.
    pub fn normalized(mut self) -> Self {
        let mut seen = std::collections::BTreeSet::new();
        self.links.retain(|link| {
            let mut pair = [link[0].trim().to_owned(), link[1].trim().to_owned()];
            pair.sort();
            seen.insert(pair)
        });
        self.links.sort();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn a_missing_file_is_an_empty_network_not_an_error() {
        let temp = tempdir().unwrap();

        let bridges = JumpBridges::load(&temp.path().join("absent.toml")).unwrap();

        assert!(bridges.links.is_empty());
    }

    #[test]
    fn a_malformed_file_is_an_error_rather_than_a_silent_empty_network() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("jump-bridges.toml");
        fs::write(&path, "links = \"not a list\"\n").unwrap();

        let error = JumpBridges::load(&path).unwrap_err().to_string();

        assert!(error.contains("could not parse"), "{error}");
    }

    #[test]
    fn an_unknown_key_is_rejected_so_a_typo_cannot_hide_the_network() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("jump-bridges.toml");
        fs::write(&path, "linkz = [[\"A\", \"B\"]]\n").unwrap();

        assert!(JumpBridges::load(&path).is_err());
    }

    #[test]
    fn a_blank_system_name_is_rejected() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("jump-bridges.toml");
        fs::write(&path, "links = [[\"A\", \"  \"]]\n").unwrap();

        let error = JumpBridges::load(&path).unwrap_err().to_string();

        assert!(error.contains("must name two systems"), "{error}");
    }

    #[test]
    fn saving_and_loading_round_trips() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("nested/jump-bridges.toml");
        let bridges = JumpBridges {
            source: Some("synthetic".to_owned()),
            generated_at: Some("2026-09-03T12:00:00Z".parse().unwrap()),
            links: vec![
                ["HOME-1".to_owned(), "FAR-4".to_owned()],
                ["NEAR-2".to_owned(), "MID-3".to_owned()],
            ],
        };

        bridges.save(&path).unwrap();
        let loaded = JumpBridges::load(&path).unwrap();

        assert_eq!(loaded.links.len(), 2);
        assert_eq!(loaded.source.as_deref(), Some("synthetic"));
        // The comment header survives for whoever opens the file next.
        assert!(fs::read_to_string(&path).unwrap().starts_with('#'));
    }

    #[test]
    fn normalizing_collapses_reversed_duplicates() {
        let bridges = JumpBridges {
            links: vec![
                ["NEAR-2".to_owned(), "MID-3".to_owned()],
                // RIFT stores both directions; they are one bridge.
                ["MID-3".to_owned(), "NEAR-2".to_owned()],
                ["HOME-1".to_owned(), "FAR-4".to_owned()],
                [" HOME-1 ".to_owned(), "FAR-4".to_owned()],
            ],
            ..JumpBridges::default()
        }
        .normalized();

        assert_eq!(bridges.links.len(), 2);
        // Sorted, so regenerating the file produces no spurious diff.
        assert_eq!(bridges.links[0][0], "HOME-1");
    }
}
