use std::{
    path::{Path, PathBuf},
    sync::LazyLock,
};

use chrono::{DateTime, NaiveDateTime, Utc};
use regex::Regex;

static CHAT_LOG_FILENAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?<channel>.+)_(?<date>[0-9]{8})_(?<time>[0-9]{6})_(?<character>[0-9]+)\.txt$")
        .expect("chat log filename regex must compile")
});

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChatLogFile {
    pub path: PathBuf,
    pub channel: String,
    pub character_id: u64,
    pub session_started: DateTime<Utc>,
}

impl ChatLogFile {
    pub fn parse(path: impl AsRef<Path>) -> Option<Self> {
        let path = path.as_ref();
        if path.is_dir() {
            return None;
        }

        let filename = path.file_name()?.to_str()?;
        let captures = CHAT_LOG_FILENAME.captures(filename)?;
        let channel = captures.name("channel")?.as_str().to_owned();
        let character_id = captures.name("character")?.as_str().parse().ok()?;
        let timestamp = format!(
            "{}{}",
            captures.name("date")?.as_str(),
            captures.name("time")?.as_str()
        );
        let session_started = NaiveDateTime::parse_from_str(&timestamp, "%Y%m%d%H%M%S")
            .ok()?
            .and_utc();

        Some(Self {
            path: path.to_path_buf(),
            channel,
            character_id,
            session_started,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_names_with_spaces_and_punctuation() {
        let parsed =
            ChatLogFile::parse("/tmp/Imperium Expeditions.test_20260903_083744_2112345678.txt")
                .unwrap();

        assert_eq!(parsed.channel, "Imperium Expeditions.test");
        assert_eq!(parsed.character_id, 2_112_345_678);
        assert_eq!(
            parsed.session_started,
            "2026-09-03T08:37:44Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn rejects_old_files_without_a_character_id() {
        assert!(ChatLogFile::parse("/tmp/Local_20260903_083744.txt").is_none());
    }

    #[test]
    fn rejects_invalid_calendar_dates() {
        assert!(ChatLogFile::parse("/tmp/Local_20261340_256199_2112345678.txt").is_none());
    }
}
