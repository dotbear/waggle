//! Whether a newer Waggle has been released.
//!
//! Asks GitHub's public releases API for the latest release of the Waggle
//! repository, at most once a day. Nothing is sent but an ordinary request
//! with Waggle's User-Agent; no data about the user or their characters.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

const LATEST: &str = "https://api.github.com/repos/dotbear/waggle/releases/latest";

/// A release newer than this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    pub url: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// The latest release if it is newer than `current`, or `None`.
pub async fn check(user_agent: &str, current: &str) -> Result<Option<Update>> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(15))
        .build()
        .context("could not build the update client")?;
    let response = client
        .get(LATEST)
        .send()
        .await
        .context("could not ask GitHub")?;
    // No release published yet.
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let release: Release = response
        .error_for_status()
        .context("GitHub refused the update check")?
        .json()
        .await
        .context("GitHub's answer was not a release")?;
    Ok(newer(&release, current))
}

fn newer(release: &Release, current: &str) -> Option<Update> {
    if release.draft || release.prerelease || !release.html_url.starts_with("https://github.com/") {
        return None;
    }
    let version = release.tag_name.trim_start_matches('v');
    (parse(version)? > parse(current)?).then(|| Update {
        version: version.to_owned(),
        url: release.html_url.clone(),
    })
}

/// `major.minor.patch`, ignoring any suffix after the patch number.
fn parse(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.to_owned(),
            html_url: format!("https://github.com/dotbear/waggle/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
        }
    }

    #[test]
    fn only_a_newer_release_is_an_update() {
        assert_eq!(newer(&release("v0.2.0"), "0.1.0").unwrap().version, "0.2.0");
        assert_eq!(newer(&release("v0.1.1"), "0.1.0").unwrap().version, "0.1.1");
        assert!(newer(&release("v0.1.0"), "0.1.0").is_none());
        assert!(newer(&release("v0.0.9"), "0.1.0").is_none());
        assert_eq!(newer(&release("v1.0.0"), "0.9.9").unwrap().version, "1.0.0");
    }

    #[test]
    fn drafts_prereleases_and_odd_links_are_ignored() {
        let mut draft = release("v9.0.0");
        draft.draft = true;
        assert!(newer(&draft, "0.1.0").is_none());
        let mut pre = release("v9.0.0");
        pre.prerelease = true;
        assert!(newer(&pre, "0.1.0").is_none());
        let mut elsewhere = release("v9.0.0");
        elsewhere.html_url = "https://example.invalid/waggle".to_owned();
        assert!(newer(&elsewhere, "0.1.0").is_none());
    }

    #[test]
    fn versions_parse_leniently() {
        assert_eq!(parse("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse("1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse("not.a.version"), None);
    }
}
