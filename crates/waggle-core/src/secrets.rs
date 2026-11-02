use std::{
    io::Write,
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use secrecy::{ExposeSecret, SecretString};

const REFRESH_TOKEN_KIND: &str = "eve-sso-refresh-token";
const JABBER_PASSWORD_KIND: &str = "jabber-password";

pub trait RefreshTokenStore {
    fn store(&self, character_id: u64, character_name: &str, token: &SecretString) -> Result<()>;
    fn lookup(&self, character_id: u64) -> Result<Option<SecretString>>;
    fn delete(&self, character_id: u64) -> Result<()>;
}

pub trait JabberPasswordStore {
    fn store_jabber_password(&self, jid: &str, password: &SecretString) -> Result<()>;
    fn lookup_jabber_password(&self, jid: &str) -> Result<Option<SecretString>>;
    fn delete_jabber_password(&self, jid: &str) -> Result<()>;
}

/// The desktop Secret Service, through `secret-tool` (libsecret's CLI).
///
/// Entries are tagged with the application's own attribute, so Waggle and
/// EVEmarchy can share one keyring without reading each other's tokens.
#[derive(Debug, Clone, Copy)]
pub struct SecretTool {
    /// The `application` attribute every entry carries, such as `waggle`.
    application: &'static str,
    /// The product name in each entry's human-readable label.
    product: &'static str,
}

impl SecretTool {
    pub const fn new(application: &'static str, product: &'static str) -> Self {
        Self {
            application,
            product,
        }
    }
}

impl RefreshTokenStore for SecretTool {
    fn store(&self, character_id: u64, character_name: &str, token: &SecretString) -> Result<()> {
        let label = format!("{} EVE SSO — {character_name}", self.product);
        let mut child = Command::new("secret-tool")
            .args([
                "store",
                "--label",
                &label,
                "application",
                self.application,
                "kind",
                REFRESH_TOKEN_KIND,
                "character_id",
                &character_id.to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not start secret-tool")?;

        child
            .stdin
            .take()
            .context("secret-tool did not provide standard input")?
            .write_all(token.expose_secret().as_bytes())
            .context("could not send the refresh token to Secret Service")?;

        let output = child
            .wait_with_output()
            .context("could not wait for secret-tool")?;
        if !output.status.success() {
            bail!(
                "Secret Service refused the refresh token: {}",
                sanitized_stderr(&output.stderr)
            );
        }
        Ok(())
    }

    fn lookup(&self, character_id: u64) -> Result<Option<SecretString>> {
        let output = Command::new("secret-tool")
            .args([
                "lookup",
                "application",
                self.application,
                "kind",
                REFRESH_TOKEN_KIND,
                "character_id",
                &character_id.to_string(),
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("could not start secret-tool")?;

        if !output.status.success() {
            if output.stderr.iter().all(u8::is_ascii_whitespace) {
                return Ok(None);
            }
            bail!(
                "Secret Service could not look up the refresh token: {}",
                sanitized_stderr(&output.stderr)
            );
        }
        let value = String::from_utf8(output.stdout)
            .context("Secret Service returned a non-UTF-8 refresh token")?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(SecretString::from(value.to_owned())))
        }
    }

    fn delete(&self, character_id: u64) -> Result<()> {
        let output = Command::new("secret-tool")
            .args([
                "clear",
                "application",
                self.application,
                "kind",
                REFRESH_TOKEN_KIND,
                "character_id",
                &character_id.to_string(),
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("could not start secret-tool")?;

        if !output.status.success() {
            if output.stderr.iter().all(u8::is_ascii_whitespace) {
                return Ok(());
            }
            bail!(
                "Secret Service could not remove the refresh token: {}",
                sanitized_stderr(&output.stderr)
            );
        }
        Ok(())
    }
}

impl JabberPasswordStore for SecretTool {
    fn store_jabber_password(&self, jid: &str, password: &SecretString) -> Result<()> {
        let label = format!("{} Jabber — {jid}", self.product);
        let mut child = Command::new("secret-tool")
            .args([
                "store",
                "--label",
                &label,
                "application",
                self.application,
                "kind",
                JABBER_PASSWORD_KIND,
                "jid",
                jid,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not start secret-tool")?;

        child
            .stdin
            .take()
            .context("secret-tool did not provide standard input")?
            .write_all(password.expose_secret().as_bytes())
            .context("could not send the Jabber password to Secret Service")?;

        let output = child
            .wait_with_output()
            .context("could not wait for secret-tool")?;
        if !output.status.success() {
            bail!(
                "Secret Service refused the Jabber password: {}",
                sanitized_stderr(&output.stderr)
            );
        }
        Ok(())
    }

    fn lookup_jabber_password(&self, jid: &str) -> Result<Option<SecretString>> {
        let output = Command::new("secret-tool")
            .args([
                "lookup",
                "application",
                self.application,
                "kind",
                JABBER_PASSWORD_KIND,
                "jid",
                jid,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("could not start secret-tool")?;

        if !output.status.success() {
            if output.stderr.iter().all(u8::is_ascii_whitespace) {
                return Ok(None);
            }
            bail!(
                "Secret Service could not look up the Jabber password: {}",
                sanitized_stderr(&output.stderr)
            );
        }
        let value = String::from_utf8(output.stdout)
            .context("Secret Service returned a non-UTF-8 Jabber password")?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(SecretString::from(value.to_owned())))
        }
    }

    fn delete_jabber_password(&self, jid: &str) -> Result<()> {
        let output = Command::new("secret-tool")
            .args([
                "clear",
                "application",
                self.application,
                "kind",
                JABBER_PASSWORD_KIND,
                "jid",
                jid,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("could not start secret-tool")?;

        if !output.status.success() {
            if output.stderr.iter().all(u8::is_ascii_whitespace) {
                return Ok(());
            }
            bail!(
                "Secret Service could not remove the Jabber password: {}",
                sanitized_stderr(&output.stderr)
            );
        }
        Ok(())
    }
}

fn sanitized_stderr(stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr);
    let detail = detail.trim();
    if detail.is_empty() {
        "secret-tool exited unsuccessfully".to_owned()
    } else {
        detail.to_owned()
    }
}

/// Secrets held in memory only, for tests and for a front end that has not
/// wired up a keyring yet. Nothing survives the process.
#[derive(Debug, Default)]
pub struct MemoryStore {
    tokens: std::sync::Mutex<std::collections::HashMap<u64, SecretString>>,
    passwords: std::sync::Mutex<std::collections::HashMap<String, SecretString>>,
}

impl RefreshTokenStore for MemoryStore {
    fn store(&self, character_id: u64, _character_name: &str, token: &SecretString) -> Result<()> {
        self.tokens
            .lock()
            .expect("memory secret store poisoned")
            .insert(character_id, token.clone());
        Ok(())
    }

    fn lookup(&self, character_id: u64) -> Result<Option<SecretString>> {
        Ok(self
            .tokens
            .lock()
            .expect("memory secret store poisoned")
            .get(&character_id)
            .cloned())
    }

    fn delete(&self, character_id: u64) -> Result<()> {
        self.tokens
            .lock()
            .expect("memory secret store poisoned")
            .remove(&character_id);
        Ok(())
    }
}

impl JabberPasswordStore for MemoryStore {
    fn store_jabber_password(&self, jid: &str, password: &SecretString) -> Result<()> {
        self.passwords
            .lock()
            .expect("memory secret store poisoned")
            .insert(jid.to_owned(), password.clone());
        Ok(())
    }

    fn lookup_jabber_password(&self, jid: &str) -> Result<Option<SecretString>> {
        Ok(self
            .passwords
            .lock()
            .expect("memory secret store poisoned")
            .get(jid)
            .cloned())
    }

    fn delete_jabber_password(&self, jid: &str) -> Result<()> {
        self.passwords
            .lock()
            .expect("memory secret store poisoned")
            .remove(jid);
        Ok(())
    }
}
