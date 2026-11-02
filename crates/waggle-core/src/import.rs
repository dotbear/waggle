//! One-shot import of settings from another intel tool.
//!
//! This is deliberately a migration, not an integration. The plan rejected
//! reading another application's credential store at runtime, and that still
//! holds: this reads the other tool's file once, writes what it finds into
//! EVEmarchy's own configuration and the desktop keyring, and never looks at it
//! again. Nothing here runs from the daemon.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use secrecy::SecretString;
use serde::Deserialize;

use crate::{bridges::JumpBridges, secrets::JabberPasswordStore};

/// The subset of RIFT's settings file that EVEmarchy can use.
///
/// Deliberately partial: `serde` ignores everything else, so the OAuth tokens
/// and unrelated UI state in that file are never read into memory.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RiftSettings {
    /// The account name only. RIFT stores the XMPP domain separately from the
    /// user's own settings, so the caller has to supply it.
    jabber_jid_local_part: Option<String>,
    jabber_password: Option<String>,
    /// Stored as a directed map that happens to be symmetric; normalized on
    /// the way out rather than trusted to stay that way.
    #[serde(default)]
    jump_bridge_network: BTreeMap<String, String>,
    #[serde(default)]
    intel_channels: Vec<RiftIntelChannel>,
}

#[derive(Debug, Deserialize)]
struct RiftIntelChannel {
    name: String,
    region: Option<String>,
}

/// What an import found and what it did with it.
#[derive(Debug, Default)]
pub struct Imported {
    pub bridges: usize,
    pub bridge_warnings: Vec<String>,
    pub jabber_jid: Option<String>,
    /// RIFT's Jabber username, whether or not a password came with it, so a
    /// front end can fill it in and ask only for the password.
    pub jabber_user: Option<String>,
    pub jabber_password_stored: bool,
    /// Channel name to region hints, ready to paste into `config.toml`.
    pub channels: BTreeMap<String, Vec<String>>,
    pub notes: Vec<String>,
}

/// Reads RIFT's settings and writes what EVEmarchy can use.
///
/// `jabber_domain` is required to import the credential: RIFT keeps only the
/// local part of the JID, and the password has to be stored under the exact
/// full JID the daemon will later look up. Without it the Jabber half is
/// skipped and the rest still imports.
/// Where RIFT keeps its settings for this user, whether or not it exists:
/// `%APPDATA%\\RIFT\\settings.json` on Windows, `~/.config/RIFT/settings.json`
/// on Linux (following `XDG_CONFIG_HOME`). RIFT names its folder after itself
/// inside the platform's config directory.
pub fn default_rift_settings() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|dirs| dirs.config_dir().join("RIFT").join("settings.json"))
}

pub fn from_rift(
    settings_file: &Path,
    jump_bridges_file: &Path,
    jabber_domain: Option<&str>,
    secrets: &(impl JabberPasswordStore + ?Sized),
    dry_run: bool,
) -> Result<Imported> {
    if !settings_file.exists() {
        bail!(
            "no RIFT settings at {}; pass --settings with the path to settings.json",
            settings_file.display()
        );
    }
    let source = fs::read_to_string(settings_file)
        .with_context(|| format!("could not read {}", settings_file.display()))?;
    let settings: RiftSettings = serde_json::from_str(&source)
        .with_context(|| format!("could not parse {}", settings_file.display()))?;

    let mut imported = Imported::default();

    // --- jump bridges --------------------------------------------------------
    let links = settings
        .jump_bridge_network
        .iter()
        .map(|(from, to)| [from.clone(), to.clone()])
        .collect::<Vec<_>>();
    if links.is_empty() {
        imported
            .notes
            .push("RIFT has no jump-bridge network to import".to_owned());
    } else {
        let network = JumpBridges {
            source: Some(format!("RIFT ({})", settings_file.display())),
            generated_at: Some(Utc::now()),
            links,
        }
        .normalized();
        imported.bridges = network.links.len();
        if !dry_run {
            network.save(jump_bridges_file)?;
        }
    }

    // --- jabber credential ---------------------------------------------------
    imported.jabber_user = settings
        .jabber_jid_local_part
        .clone()
        .filter(|local| !local.trim().is_empty());
    match (
        settings.jabber_jid_local_part.as_deref(),
        settings.jabber_password.as_deref(),
        jabber_domain,
    ) {
        (Some(local), Some(password), Some(domain))
            if !local.is_empty() && !password.is_empty() =>
        {
            let domain = domain.trim().trim_start_matches('@');
            // A malformed JID would be stored under a key the daemon never
            // looks up, so the credential would silently not be found.
            if domain.is_empty() || domain.contains('@') || domain.contains(char::is_whitespace) {
                bail!("the Jabber domain must be a bare domain such as example.com");
            }
            let jid = format!("{local}@{domain}");
            if !dry_run {
                secrets
                    .store_jabber_password(&jid, &SecretString::from(password.to_owned()))
                    .context("could not store the Jabber password in the keyring")?;
                imported.jabber_password_stored = true;
            }
            imported.jabber_jid = Some(jid);
        }
        (Some(local), Some(_), None) if !local.is_empty() => imported.notes.push(
            "found a Jabber credential but no Jabber domain, so it was skipped; \
             give the part after the @ of your Jabber account (--jabber-domain)"
                .to_owned(),
        ),
        _ => imported
            .notes
            .push("RIFT has no stored Jabber password to import.".to_owned()),
    }

    // --- intel channels ------------------------------------------------------
    for channel in settings.intel_channels {
        if channel.name.trim().is_empty() {
            continue;
        }
        let regions = imported.channels.entry(channel.name).or_default();
        if let Some(region) = channel.region.filter(|region| !region.trim().is_empty())
            && !regions.contains(&region)
        {
            regions.push(region);
        }
    }

    if settings.jabber_password.is_some() {
        imported.notes.push(format!(
            "{} stores its EVE tokens and Jabber password in plain text; \
             consider clearing it once EVEmarchy is working",
            settings_file.display()
        ));
    }

    Ok(imported)
}

/// Renders the parts that belong in the hand-edited `config.toml`.
///
/// Printed rather than written: `config.toml` carries the operator's own
/// comments and choices, and rewriting it through a serializer would discard
/// them. TOML serialization is used for the escaping, so a channel or JID
/// containing a quote cannot break out of its string.
pub fn config_snippet(imported: &Imported, directorbot: Option<&str>) -> Result<String> {
    #[derive(serde::Serialize)]
    struct Snippet {
        #[serde(skip_serializing_if = "Option::is_none")]
        jabber: Option<JabberSnippet>,
        #[serde(skip_serializing_if = "Option::is_none")]
        intel: Option<IntelSnippet>,
    }

    #[derive(serde::Serialize)]
    struct JabberSnippet {
        jid: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        directorbot: Option<String>,
    }

    #[derive(serde::Serialize)]
    struct IntelSnippet {
        channels: BTreeMap<String, Vec<String>>,
    }

    let snippet = Snippet {
        jabber: imported.jabber_jid.clone().map(|jid| JabberSnippet {
            jid,
            directorbot: directorbot.map(str::to_owned),
        }),
        intel: (!imported.channels.is_empty()).then(|| IntelSnippet {
            channels: imported.channels.clone(),
        }),
    };

    toml::to_string_pretty(&snippet).context("could not render the configuration snippet")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sections a snippet may write, refusing anything else, as a front
    /// end's own configuration does.
    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SnippetConfig {
        #[serde(default)]
        jabber: crate::config::JabberConfig,
        #[serde(default)]
        intel: crate::config::IntelConfig,
    }
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Records what would have gone into the keyring.
    #[derive(Default)]
    struct FakeSecrets {
        stored: Mutex<Vec<(String, String)>>,
    }

    impl JabberPasswordStore for FakeSecrets {
        fn store_jabber_password(&self, jid: &str, password: &SecretString) -> Result<()> {
            use secrecy::ExposeSecret;
            self.stored
                .lock()
                .unwrap()
                .push((jid.to_owned(), password.expose_secret().to_owned()));
            Ok(())
        }

        fn lookup_jabber_password(&self, _jid: &str) -> Result<Option<SecretString>> {
            Ok(None)
        }

        fn delete_jabber_password(&self, _jid: &str) -> Result<()> {
            Ok(())
        }
    }

    /// Shaped like RIFT's file, with invented values throughout.
    fn settings_json() -> &'static str {
        r#"{
          "jabberJidLocalPart": "synthetic_pilot",
          "jabberPassword": "synthetic-secret",
          "jumpBridgeNetwork": {
            "HOME-1": "FAR-4",
            "FAR-4": "HOME-1",
            "NEAR-2": "MID-3",
            "MID-3": "NEAR-2"
          },
          "intelChannels": [
            {"name": "one.synthetic", "region": "Region One"},
            {"name": "two.synthetic", "region": "Region Two"},
            {"name": "two.synthetic", "region": "Region Three"}
          ],
          "authenticatedCharacters": {"1": {"accessToken": "must-not-be-read"}}
        }"#
    }

    fn write_settings(directory: &Path, body: &str) -> std::path::PathBuf {
        let path = directory.join("settings.json");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn imports_bridges_credentials_and_channels() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), settings_json());
        let bridges_file = temp.path().join("jump-bridges.toml");
        let secrets = FakeSecrets::default();

        let imported = from_rift(
            &settings,
            &bridges_file,
            Some("example.invalid"),
            &secrets,
            false,
        )
        .unwrap();

        // Both directions of each bridge collapse into one link.
        assert_eq!(imported.bridges, 2);
        let saved = JumpBridges::load(&bridges_file).unwrap();
        assert_eq!(saved.links.len(), 2);
        assert!(saved.source.unwrap().starts_with("RIFT ("));

        assert_eq!(
            imported.jabber_jid.as_deref(),
            Some("synthetic_pilot@example.invalid")
        );
        assert!(imported.jabber_password_stored);
        let stored = secrets.stored.lock().unwrap();
        assert_eq!(stored[0].0, "synthetic_pilot@example.invalid");
        assert_eq!(stored[0].1, "synthetic-secret");

        // A channel listed twice keeps both of its regions.
        assert_eq!(imported.channels["two.synthetic"].len(), 2);
        assert_eq!(imported.channels["one.synthetic"], vec!["Region One"]);
    }

    #[test]
    fn a_dry_run_writes_nothing_anywhere() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), settings_json());
        let bridges_file = temp.path().join("jump-bridges.toml");
        let secrets = FakeSecrets::default();

        let imported = from_rift(
            &settings,
            &bridges_file,
            Some("example.invalid"),
            &secrets,
            true,
        )
        .unwrap();

        assert_eq!(imported.bridges, 2, "it still reports what it would do");
        assert!(!imported.jabber_password_stored);
        assert!(!bridges_file.exists());
        assert!(secrets.stored.lock().unwrap().is_empty());
    }

    #[test]
    fn without_a_domain_the_credential_is_skipped_but_the_rest_imports() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), settings_json());
        let secrets = FakeSecrets::default();

        let imported = from_rift(
            &settings,
            &temp.path().join("jump-bridges.toml"),
            None,
            &secrets,
            false,
        )
        .unwrap();

        assert!(imported.jabber_jid.is_none());
        assert!(!imported.jabber_password_stored);
        assert_eq!(imported.bridges, 2);
        assert!(
            imported
                .notes
                .iter()
                .any(|note| note.contains("--jabber-domain"))
        );
    }

    #[test]
    fn a_malformed_domain_is_refused_rather_than_stored_wrong() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), settings_json());
        let secrets = FakeSecrets::default();

        for domain in ["", "  ", "user@example.invalid", "example .invalid"] {
            let error = from_rift(
                &settings,
                &temp.path().join("jump-bridges.toml"),
                Some(domain),
                &secrets,
                false,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("bare domain"), "{domain:?}: {error}");
        }
    }

    #[test]
    fn a_leading_at_sign_in_the_domain_is_tolerated() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), settings_json());
        let secrets = FakeSecrets::default();

        let imported = from_rift(
            &settings,
            &temp.path().join("jump-bridges.toml"),
            Some("@example.invalid"),
            &secrets,
            false,
        )
        .unwrap();

        assert_eq!(
            imported.jabber_jid.as_deref(),
            Some("synthetic_pilot@example.invalid")
        );
    }

    #[test]
    fn the_plaintext_credential_warning_names_the_file() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), settings_json());

        let imported = from_rift(
            &settings,
            &temp.path().join("jump-bridges.toml"),
            Some("example.invalid"),
            &FakeSecrets::default(),
            true,
        )
        .unwrap();

        assert!(
            imported
                .notes
                .iter()
                .any(|note| note.contains("plain text") && note.contains("settings.json"))
        );
    }

    #[test]
    fn a_missing_settings_file_says_how_to_point_at_one() {
        let temp = tempdir().unwrap();

        let error = from_rift(
            &temp.path().join("absent.json"),
            &temp.path().join("jump-bridges.toml"),
            None,
            &FakeSecrets::default(),
            false,
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("--settings"), "{error}");
    }

    #[test]
    fn an_empty_rift_install_imports_nothing_and_says_so() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), "{}");

        let imported = from_rift(
            &settings,
            &temp.path().join("jump-bridges.toml"),
            Some("example.invalid"),
            &FakeSecrets::default(),
            false,
        )
        .unwrap();

        assert_eq!(imported.bridges, 0);
        assert!(imported.channels.is_empty());
        assert_eq!(imported.notes.len(), 2);
    }

    #[test]
    fn the_snippet_is_valid_toml_that_parses_back_into_the_shared_config() {
        let temp = tempdir().unwrap();
        let settings = write_settings(temp.path(), settings_json());
        let imported = from_rift(
            &settings,
            &temp.path().join("jump-bridges.toml"),
            Some("example.invalid"),
            &FakeSecrets::default(),
            true,
        )
        .unwrap();

        let snippet = config_snippet(&imported, Some("directorbot@example.invalid")).unwrap();
        let config: SnippetConfig = toml::from_str(&snippet).unwrap();

        assert_eq!(
            config.jabber.jid.as_deref(),
            Some("synthetic_pilot@example.invalid")
        );
        assert_eq!(
            config.jabber.directorbot.as_deref(),
            Some("directorbot@example.invalid")
        );
        assert_eq!(config.intel.channels.len(), 2);
        config.jabber.validate().unwrap();
        config.intel.validate().unwrap();
    }

    #[test]
    fn a_channel_name_containing_a_quote_cannot_break_the_snippet() {
        let temp = tempdir().unwrap();
        let settings = write_settings(
            temp.path(),
            r#"{"intelChannels": [{"name": "od\"d.synthetic", "region": "R"}]}"#,
        );
        let imported = from_rift(
            &settings,
            &temp.path().join("jump-bridges.toml"),
            None,
            &FakeSecrets::default(),
            true,
        )
        .unwrap();

        let snippet = config_snippet(&imported, None).unwrap();
        let config: SnippetConfig = toml::from_str(&snippet).unwrap();

        assert!(config.intel.channels.contains_key("od\"d.synthetic"));
    }

    #[test]
    fn rift_lives_in_the_platforms_config_directory() {
        let path = default_rift_settings().unwrap();
        assert!(
            path.ends_with("RIFT/settings.json") || path.ends_with("RIFT\\settings.json"),
            "{}",
            path.display()
        );
    }
}
