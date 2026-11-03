//! Whether a newer Waggle has been released.
//!
//! Asks GitHub's public releases API for the Waggle repository's recent
//! releases, at most once a day. Before 1.0 every release is a pre-release,
//! so a 0.x build counts pre-releases too; from 1.0 on only full releases
//! count. Nothing is sent but an ordinary request
//! with Waggle's User-Agent; no data about the user or their characters.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

const RELEASES: &str = "https://api.github.com/repos/dotbear/waggle/releases?per_page=20";

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

/// The newest release if it is newer than `current`, or `None`.
pub async fn check(user_agent: &str, current: &str) -> Result<Option<Update>> {
    let client = reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(15))
        .build()
        .context("could not build the update client")?;
    let response = client
        .get(RELEASES)
        .send()
        .await
        .context("could not ask GitHub")?;
    let releases: Vec<Release> = response
        .error_for_status()
        .context("GitHub refused the update check")?
        .json()
        .await
        .context("GitHub's answer was not a list of releases")?;
    Ok(newest(&releases, current))
}

/// The highest-numbered release above `current` that this build should be
/// told about.
fn newest(releases: &[Release], current: &str) -> Option<Update> {
    let current = parse(current)?;
    let pre_one = current.0 == 0;
    releases
        .iter()
        .filter(|r| !r.draft && (pre_one || !r.prerelease))
        .filter(|r| r.html_url.starts_with("https://github.com/"))
        .filter_map(|r| {
            let version = r.tag_name.trim_start_matches('v');
            Some((parse(version)?, version, r))
        })
        .filter(|(v, _, _)| *v > current)
        .max_by_key(|(v, _, _)| *v)
        .map(|(_, version, r)| Update {
            version: version.to_owned(),
            url: r.html_url.clone(),
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

    fn pre(tag: &str) -> Release {
        Release {
            prerelease: true,
            ..release(tag)
        }
    }

    fn found(releases: &[Release], current: &str) -> Option<String> {
        newest(releases, current).map(|u| u.version)
    }

    #[test]
    fn only_a_newer_release_is_an_update() {
        assert_eq!(
            found(&[release("v0.2.0")], "0.1.0").as_deref(),
            Some("0.2.0")
        );
        assert_eq!(
            found(&[release("v0.1.1")], "0.1.0").as_deref(),
            Some("0.1.1")
        );
        assert_eq!(found(&[release("v0.1.0")], "0.1.0"), None);
        assert_eq!(found(&[release("v0.0.9")], "0.1.0"), None);
        assert_eq!(
            found(&[release("v1.0.0")], "0.9.9").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(found(&[], "0.1.0"), None);
    }

    #[test]
    fn the_highest_version_wins_whatever_the_order() {
        let releases = [release("v0.1.1"), release("v0.3.0"), release("v0.2.0")];
        assert_eq!(found(&releases, "0.1.0").as_deref(), Some("0.3.0"));
    }

    #[test]
    fn pre_releases_count_only_before_one_point_oh() {
        assert_eq!(found(&[pre("v0.2.0")], "0.1.0").as_deref(), Some("0.2.0"));
        assert_eq!(found(&[pre("v1.1.0")], "1.0.0"), None);
        let releases = [release("v1.1.0"), pre("v1.2.0")];
        assert_eq!(found(&releases, "1.0.0").as_deref(), Some("1.1.0"));
    }

    #[test]
    fn drafts_and_odd_links_are_ignored() {
        let draft = Release {
            draft: true,
            ..release("v9.0.0")
        };
        assert_eq!(found(&[draft], "0.1.0"), None);
        let mut elsewhere = release("v9.0.0");
        elsewhere.html_url = "https://example.invalid/waggle".to_owned();
        assert_eq!(found(&[elsewhere], "0.1.0"), None);
    }

    #[test]
    fn versions_parse_leniently() {
        assert_eq!(parse("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse("1.2.3-beta"), Some((1, 2, 3)));
        assert_eq!(parse("not.a.version"), None);
    }
}
