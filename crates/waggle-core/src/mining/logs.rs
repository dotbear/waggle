//! Game logs have their own byte encoding and header; chat offsets are unrelated.
use anyhow::{Result, bail, ensure};
use chrono::NaiveDateTime;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    sync::LazyLock,
};

static TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
static YIELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:You mined ([0-9][0-9,]*) units of (.+)|Critical mining success!\s*You mined an additional ([0-9][0-9,]*) units of (.+))$").unwrap()
});
/// Residue is ore the laser destroys, not ore anyone collects.
static RESIDUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Additional\s+[0-9][0-9,]*\s+units depleted from asteroid as residue$").unwrap()
});
#[derive(Debug, Clone)]
pub struct Event {
    pub at: i64,
    pub ore: String,
    pub units: i64,
    pub bonus: bool,
}
/// The timestamp, the channel EVE filed the line under (`mining`, `notify`,
/// `combat`, `None`…) and the message itself, with EVE's markup taken back out.
pub fn entry(line: &str) -> Option<(i64, &str, String)> {
    let (stamp, rest) = line
        .trim_start_matches('\u{feff}')
        .strip_prefix("[ ")?
        .split_once(" ] (")?;
    let (kind, message) = rest.split_once(") ")?;
    let at = NaiveDateTime::parse_from_str(stamp, "%Y.%m.%d %H:%M:%S")
        .ok()?
        .and_utc()
        .timestamp();
    Some((at, kind, TAG.replace_all(message, "").trim().to_owned()))
}
/// The timestamp and text of a `(mining)` line.
fn message(line: &str) -> Option<(i64, String)> {
    entry(line).and_then(|(at, kind, text)| (kind == "mining").then_some((at, text)))
}

/// A mining message this meter understands and deliberately does not count.
///
/// Counting residue would inflate every total, and calling it unreadable would
/// mark the coverage of every real mining session incomplete. It is neither.
pub fn understood_without_yield(line: &str) -> bool {
    message(line).is_some_and(|(_, text)| RESIDUE.is_match(&text))
}

pub fn parse(line: &str) -> Option<Event> {
    let (at, text) = message(line)?;
    let captures = YIELD.captures(&text)?;
    let bonus = captures.get(3).is_some();
    let units: i64 = captures
        .get(if bonus { 3 } else { 1 })?
        .as_str()
        .replace(',', "")
        .parse()
        .ok()?;
    if units <= 0 {
        return None;
    }
    Some(Event {
        at,
        ore: captures
            .get(if bonus { 4 } else { 2 })?
            .as_str()
            .trim()
            .to_owned(),
        units,
        bonus,
    })
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub id: Option<String>,
    pub start: i64,
    pub utf16: bool,
}
/// `Ok(None)` is a complete gamelog that belongs to no character.
///
/// EVE writes one of these every time the client reaches character select, and
/// it never gains a `Listener:` line. Waiting for one forever would report a
/// permanent problem about a file that is already finished.
pub fn header(path: &Path) -> Result<Option<Header>> {
    let mut bytes = vec![];
    File::open(path)?.take(65536).read_to_end(&mut bytes)?;
    let utf16 = bytes.starts_with(&[255, 254]);
    // The read limit may split a multibyte character far after the header.
    let complete = if utf16 {
        bytes
            .chunks_exact(2)
            .rposition(|b| b == [10, 0])
            .map(|n| (n + 1) * 2)
    } else {
        bytes.iter().rposition(|b| *b == 10).map(|n| n + 1)
    };
    let text = decode(&bytes[..complete.unwrap_or(0)], utf16)?;
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key).map(|s| s.trim().to_owned()))
    };
    let start = value("Session Started:");
    let Some(name) = value("Listener:") else {
        // Two rules mean the header block closed: EVE opens and closes it with
        // a separator line. Anything shorter is still being written.
        let closed = text
            .lines()
            .filter(|l| l.trim_start().starts_with("-----"))
            .count()
            >= 2;
        anyhow::ensure!(
            closed && start.is_some(),
            "waiting for game-log Listener header"
        );
        return Ok(None);
    };
    let start = start.ok_or_else(|| anyhow::anyhow!("waiting for game-log session header"))?;
    let start = NaiveDateTime::parse_from_str(&start, "%Y.%m.%d %H:%M:%S")?
        .and_utc()
        .timestamp();
    let stem = path.file_stem().and_then(|v| v.to_str()).unwrap_or("");
    let parts = stem.split('_').collect::<Vec<_>>();
    let id = (parts.len() == 3)
        .then(|| parts[2])
        .filter(|v| v.parse::<u64>().is_ok())
        .map(str::to_owned);
    Ok(Some(Header {
        name,
        id,
        start,
        utf16,
    }))
}
fn decode(bytes: &[u8], utf16: bool) -> Result<String> {
    if utf16 {
        ensure!(bytes.len().is_multiple_of(2), "incomplete UTF-16 data");
        let units = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>();
        Ok(String::from_utf16(&units)?)
    } else {
        Ok(std::str::from_utf8(bytes)?.to_owned())
    }
}
/// Only complete lines are consumed, so a restart re-reads an unfinished line.
pub fn read(path: &Path, offset: u64, utf16: bool) -> Result<(u64, Vec<(u64, String)>)> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![];
    f.take(256 * 1024).read_to_end(&mut bytes)?;
    let mut lines = vec![];
    let mut start = 0;
    let step = if utf16 { 2 } else { 1 };
    let mut i = 0;
    while i + step <= bytes.len() {
        if bytes[i] == b'\n' && (!utf16 || bytes[i + 1] == 0) {
            ensure!(i - start <= 65536, "game-log line exceeds 64 KiB");
            lines.push((
                offset + start as u64,
                decode(&bytes[start..i], utf16)?
                    .trim_end_matches('\r')
                    .to_owned(),
            ));
            start = i + step;
        }
        i += step;
    }
    if bytes.len() - start > 65536 {
        bail!("game-log line exceeds 64 KiB");
    }
    Ok((offset + start as u64, lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_and_bonus_are_additive_without_markup_dependency() {
        let a=parse("[ 2026.09.15 12:00:00 ] (mining) <color=x>You mined <font size=12>89 units of Pyroxeres III-Grade").unwrap();
        let b=parse("[ 2026.09.15 12:00:00 ] (mining) Critical mining success! You mined an additional 199 units of Pyroxeres III-Grade").unwrap();
        assert_eq!(a.units + b.units, 288);
        assert!(!a.bonus && b.bonus);
        assert!(parse("[ 2026.09.15 12:00:00 ] (notify) You mined 12 units of Veldspar").is_none());
    }
    #[test]
    fn residue_is_recognised_and_never_counted_as_yield() {
        // Exactly the markup EVE writes, unclosed tags and all.
        let line = "[ 2026.09.03 12:22:03 ] (mining) <color=0x77ffffff>Additional \
<font size=12><color=#ffff454b>40<color=0x77ffffff><font size=10> units \
depleted from asteroid as residue";
        assert!(parse(line).is_none(), "residue is not collected ore");
        assert!(understood_without_yield(line));
        // A real yield message is not quietly swallowed by the same check.
        assert!(!understood_without_yield(
            "[ 2026.09.03 12:22:03 ] (mining) You mined 40 units of Mercoxit"
        ));
    }
    #[test]
    fn partial_lines_resume_at_their_start_in_both_encodings() {
        for utf16 in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("log");
            let text = "first\r\nMínér\r\n";
            let bytes = if utf16 {
                text.encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            } else {
                text.as_bytes().to_vec()
            };
            std::fs::write(&p, &bytes[..bytes.len() - 1]).unwrap();
            let (offset, lines) = read(&p, 0, utf16).unwrap();
            assert_eq!(lines.len(), 1);
            std::fs::write(&p, &bytes).unwrap();
            let (_, lines) = read(&p, offset, utf16).unwrap();
            assert_eq!(lines[0].1, "Mínér");
        }
    }
}
