use std::collections::BTreeMap;

use anyhow::{Result, bail};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChatLogHeader {
    pub channel_id: String,
    pub channel_name: String,
    pub listener: String,
    pub session_started: DateTime<Utc>,
}

#[derive(Debug, Clone, Eq, PartialEq, Deserialize, Serialize)]
pub struct ChatMessage {
    pub timestamp: DateTime<Utc>,
    pub author: String,
    pub text: String,
}

pub fn decode_utf16le(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xfe, 0xff]) {
        bail!("big-endian UTF-16 is not a supported EVE log encoding");
    }
    if !bytes.len().is_multiple_of(2) {
        bail!("UTF-16LE input contains an incomplete code unit");
    }

    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_le_bytes(*chunk))
        .collect::<Vec<_>>();

    Ok(String::from_utf16_lossy(&units))
}

pub fn parse_header(text: &str) -> Option<ChatLogHeader> {
    let mut metadata = BTreeMap::new();
    let mut inside_header = false;
    let mut saw_header = false;

    for raw_line in text.lines() {
        let line = clean_line(raw_line);
        if is_header_rule(line) {
            if inside_header {
                break;
            }
            inside_header = true;
            saw_header = true;
            continue;
        }

        if !inside_header {
            continue;
        }

        if let Some((key, value)) = line.split_once(':') {
            metadata.insert(key.trim(), value.trim());
        }
    }

    if !saw_header {
        return None;
    }

    let session_started =
        NaiveDateTime::parse_from_str(metadata.get("Session started")?, "%Y.%m.%d %H:%M:%S")
            .ok()?
            .and_utc();

    Some(ChatLogHeader {
        channel_id: metadata.get("Channel ID")?.to_string(),
        channel_name: metadata.get("Channel Name")?.to_string(),
        listener: metadata.get("Listener")?.to_string(),
        session_started,
    })
}

pub fn parse_message(raw_line: &str) -> Option<ChatMessage> {
    let line = clean_line(raw_line);
    let body = line.strip_prefix("[ ")?;
    let (timestamp, remainder) = body.split_once(" ] ")?;
    let (author, text) = remainder.split_once(" > ")?;
    let timestamp = NaiveDateTime::parse_from_str(timestamp, "%Y.%m.%d %H:%M:%S")
        .ok()?
        .and_utc();

    Some(ChatMessage {
        timestamp,
        author: author.trim().to_owned(),
        text: text.to_owned(),
    })
}

fn clean_line(line: &str) -> &str {
    line.trim_end_matches('\r').trim_start_matches('\u{feff}')
}

fn is_header_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 20 && trimmed.bytes().all(|byte| byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    }

    #[test]
    fn decodes_little_endian_utf16() {
        let decoded = decode_utf16le(&utf16le("\u{feff}EVE Online\r\n")).unwrap();

        assert_eq!(decoded, "\u{feff}EVE Online\r\n");
    }

    #[test]
    fn rejects_partial_utf16_units() {
        assert!(decode_utf16le(&[0x45]).is_err());
    }

    #[test]
    fn parses_the_interoperable_header_fields() {
        let header = parse_header(
            "\u{feff}\r\n\
             ---------------------------------------------------------------\r\n\
             Channel ID:      channel-42\r\n\
             Channel Name:    example.intel\r\n\
             Listener:        Example Pilot\r\n\
             Session started: 2026.09.03 08:36:21\r\n\
             ---------------------------------------------------------------\r\n",
        )
        .unwrap();

        assert_eq!(header.channel_id, "channel-42");
        assert_eq!(header.channel_name, "example.intel");
        assert_eq!(header.listener, "Example Pilot");
        assert_eq!(
            header.session_started,
            "2026-09-03T08:36:21Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn parses_messages_with_a_bom_and_greater_than_in_the_body() {
        let message =
            parse_message("\u{feff}[ 2026.09.03 08:41:32 ] Example Scout > hostile > destination")
                .unwrap();

        assert_eq!(message.author, "Example Scout");
        assert_eq!(message.text, "hostile > destination");
        assert_eq!(
            message.timestamp,
            "2026-09-03T08:41:32Z".parse::<DateTime<Utc>>().unwrap()
        );
    }

    #[test]
    fn ignores_header_and_malformed_lines() {
        assert!(parse_message("Channel Name: example.intel").is_none());
        assert!(parse_message("[ not-a-date ] Pilot > message").is_none());
    }
}
