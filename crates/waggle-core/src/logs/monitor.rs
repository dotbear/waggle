use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use chrono::{Duration as ChronoDuration, Utc};
use serde::Serialize;

use super::{
    filename::ChatLogFile,
    format::{ChatLogHeader, ChatMessage},
    tailer::{StartPosition, Tailer},
};

const ACTIVE_SESSION_AGE: ChronoDuration = ChronoDuration::days(7);

#[derive(Debug, Clone, Serialize)]
pub struct ChatEvent {
    pub source: PathBuf,
    pub channel: String,
    pub character_id: u64,
    pub header: Option<ChatLogHeaderView>,
    pub message: ChatMessage,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatLogHeaderView {
    pub channel_id: String,
    pub channel_name: String,
    pub listener: String,
    pub session_started: chrono::DateTime<Utc>,
}

impl From<&ChatLogHeader> for ChatLogHeaderView {
    fn from(header: &ChatLogHeader) -> Self {
        Self {
            channel_id: header.channel_id.clone(),
            channel_name: header.channel_name.clone(),
            listener: header.listener.clone(),
            session_started: header.session_started,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct MonitorStats {
    pub log_roots: usize,
    pub chat_directories: usize,
    pub active_files: usize,
}

#[derive(Debug, Default)]
pub struct MonitorPoll {
    pub events: Vec<ChatEvent>,
    pub warnings: Vec<String>,
    pub stats: MonitorStats,
}

/// A durable resume point for one chat log.
///
/// The path alone is not enough identity: EVE reuses names across sessions, so
/// the session start is stored too and a mismatch means the file is a different
/// log that happens to share the path.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LogCheckpoint {
    pub path: PathBuf,
    pub channel: String,
    pub character_id: u64,
    pub session_started: chrono::DateTime<Utc>,
    pub offset: u64,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
struct ActiveKey {
    channel: String,
    character_id: u64,
}

pub struct ChatLogMonitor {
    log_roots: Vec<PathBuf>,
    chat_directories: Vec<PathBuf>,
    active: HashMap<ActiveKey, Tailer>,
    checkpoints: HashMap<PathBuf, LogCheckpoint>,
    initialized: bool,
    rescan_interval: Duration,
    last_rescan: Option<Instant>,
    /// When the directories were last read in full, and their modification
    /// times then. A new log changes its directory's time, so an unchanged
    /// one is skipped; a full read still happens every minute so sessions
    /// age out of the active set on time.
    last_full_rescan: Option<Instant>,
    directory_marks: Vec<Option<std::time::SystemTime>>,
}

/// How often the chat directories are read even when they look unchanged.
const FULL_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

impl ChatLogMonitor {
    pub fn new(log_roots: Vec<PathBuf>, rescan_interval: Duration) -> Self {
        let chat_directories = log_roots
            .iter()
            .map(|root| root.join("Chatlogs"))
            .filter(|path| path.is_dir())
            .collect();

        Self {
            log_roots,
            chat_directories,
            active: HashMap::new(),
            checkpoints: HashMap::new(),
            initialized: false,
            rescan_interval,
            last_rescan: None,
            last_full_rescan: None,
            directory_marks: Vec::new(),
        }
    }

    /// Seeds durable resume points before the first scan.
    ///
    /// Without this the daemon seeks every log to its end on startup, so
    /// anything written while it was down — during an upgrade restart, say — is
    /// never parsed. Replayed messages are still gated by the alert policy's
    /// freshness window, so only genuinely recent intel can alert.
    pub fn restore_checkpoints(&mut self, checkpoints: Vec<LogCheckpoint>) {
        self.checkpoints = checkpoints
            .into_iter()
            .map(|checkpoint| (checkpoint.path.clone(), checkpoint))
            .collect();
    }

    /// Current resume points, for the daemon to persist.
    pub fn checkpoints(&self) -> Vec<LogCheckpoint> {
        self.active
            .values()
            .map(|tailer| LogCheckpoint {
                path: tailer.file.path.clone(),
                channel: tailer.file.channel.clone(),
                character_id: tailer.file.character_id,
                session_started: tailer.file.session_started,
                offset: tailer.offset(),
            })
            .collect()
    }

    pub fn initialize(&mut self) -> MonitorPoll {
        self.rescan(StartPosition::End)
    }

    /// Channel names EVE is currently writing a log for.
    ///
    /// This answers "does a log exist for this channel", which is what tells a
    /// misspelled `[intel.channels]` key apart from a channel that simply has
    /// no traffic yet.
    pub fn active_channels(&self) -> BTreeSet<String> {
        self.active
            .values()
            .map(|tailer| {
                tailer.header.as_ref().map_or_else(
                    || tailer.file.channel.clone(),
                    |header| header.channel_name.clone(),
                )
            })
            .collect()
    }

    pub fn poll(&mut self) -> MonitorPoll {
        let mut output = if self.rescan_due() && self.directories_changed() {
            let start = if self.initialized {
                StartPosition::Beginning
            } else {
                StartPosition::End
            };
            self.rescan(start)
        } else {
            MonitorPoll {
                stats: self.stats(),
                ..MonitorPoll::default()
            }
        };

        for tailer in self.active.values_mut() {
            match tailer.read_new_messages() {
                Ok(batch) => {
                    if batch.was_truncated {
                        output.warnings.push(format!(
                            "{} was truncated; resumed from the beginning",
                            tailer.file.path.display()
                        ));
                    }
                    output
                        .events
                        .extend(batch.messages.into_iter().map(|message| {
                            ChatEvent {
                                source: tailer.file.path.clone(),
                                channel: tailer
                                    .header
                                    .as_ref()
                                    .map(|header| header.channel_name.clone())
                                    .unwrap_or_else(|| tailer.file.channel.clone()),
                                character_id: tailer.file.character_id,
                                header: tailer.header.as_ref().map(ChatLogHeaderView::from),
                                message,
                            }
                        }));
                }
                Err(error) => output.warnings.push(error.to_string()),
            }
        }

        output.stats = self.stats();
        output
    }

    pub fn rescan_now(&mut self) -> MonitorPoll {
        let start = if self.initialized {
            StartPosition::Beginning
        } else {
            StartPosition::End
        };
        self.rescan(start)
    }

    /// Whether a rescan could find anything; records the check either way.
    fn directories_changed(&mut self) -> bool {
        let marks = self
            .chat_directories
            .iter()
            .map(|directory| fs::metadata(directory).and_then(|m| m.modified()).ok())
            .collect::<Vec<_>>();
        let stale = self
            .last_full_rescan
            .is_none_or(|last| last.elapsed() >= FULL_RESCAN_INTERVAL);
        if !stale && marks == self.directory_marks {
            self.last_rescan = Some(Instant::now());
            return false;
        }
        self.directory_marks = marks;
        true
    }

    fn rescan_due(&self) -> bool {
        self.last_rescan
            .is_none_or(|last| last.elapsed() >= self.rescan_interval)
    }

    fn rescan(&mut self, new_file_start: StartPosition) -> MonitorPoll {
        let mut output = MonitorPoll::default();
        let mut newest = HashMap::<ActiveKey, ChatLogFile>::new();
        let oldest_allowed = Utc::now() - ACTIVE_SESSION_AGE;

        for directory in &self.chat_directories {
            let entries = match fs::read_dir(directory) {
                Ok(entries) => entries,
                Err(error) => {
                    output.warnings.push(format!(
                        "could not scan chat logs at {}: {error}",
                        directory.display()
                    ));
                    continue;
                }
            };

            for entry in entries {
                let path = match entry {
                    Ok(entry) => entry.path(),
                    Err(error) => {
                        output.warnings.push(format!(
                            "could not inspect an entry in {}: {error}",
                            directory.display()
                        ));
                        continue;
                    }
                };
                let Some(file) = ChatLogFile::parse(path) else {
                    continue;
                };
                if file.session_started < oldest_allowed {
                    continue;
                }

                let key = ActiveKey {
                    channel: file.channel.clone(),
                    character_id: file.character_id,
                };
                let should_replace = newest
                    .get(&key)
                    .is_none_or(|current| file.session_started > current.session_started);
                if should_replace {
                    newest.insert(key, file);
                }
            }
        }

        self.active.retain(|key, tailer| {
            newest
                .get(key)
                .is_some_and(|file| file.path == tailer.file.path)
        });

        for (key, file) in newest {
            if self.active.contains_key(&key) {
                continue;
            }
            let start = self
                .checkpoints
                .get(&file.path)
                .filter(|checkpoint| checkpoint.session_started == file.session_started)
                .map_or(new_file_start, |checkpoint| {
                    StartPosition::Offset(checkpoint.offset)
                });
            match Tailer::open(file, start) {
                Ok(tailer) => {
                    self.active.insert(key, tailer);
                }
                Err(error) => output.warnings.push(error.to_string()),
            }
        }

        self.initialized = true;
        self.last_rescan = Some(Instant::now());
        self.last_full_rescan = self.last_rescan;
        output.stats = self.stats();
        output
    }

    fn stats(&self) -> MonitorStats {
        MonitorStats {
            log_roots: self.log_roots.len(),
            chat_directories: self.chat_directories.len(),
            active_files: self.active.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs::OpenOptions, io::Write, path::Path};

    use super::*;
    use tempfile::tempdir;

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    }

    fn document(message: &str) -> Vec<u8> {
        utf16le(&format!(
            "\u{feff}\r\n\
             ---------------------------------------------------------------\r\n\
             Channel ID:      channel-42\r\n\
             Channel Name:    example.intel\r\n\
             Listener:        Example Pilot\r\n\
             Session started: 2026.09.03 08:36:21\r\n\
             ---------------------------------------------------------------\r\n\
             {message}"
        ))
    }

    fn append(path: &Path, text: &str) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(&utf16le(text))
            .unwrap();
    }

    fn filename(channel: &str, character_id: u64, seconds_from_now: i64) -> String {
        let timestamp = Utc::now() + ChronoDuration::seconds(seconds_from_now);
        format!(
            "{channel}_{}_{}_{character_id}.txt",
            timestamp.format("%Y%m%d"),
            timestamp.format("%H%M%S")
        )
    }

    #[test]
    fn initialization_suppresses_backlog_and_poll_returns_new_messages() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("EVE/logs");
        let chatlogs = root.join("Chatlogs");
        fs::create_dir_all(&chatlogs).unwrap();
        let path = chatlogs.join(filename("example.intel", 42, 0));
        fs::write(
            &path,
            document("\u{feff}[ 2026.09.03 08:40:00 ] Scout > old\r\n"),
        )
        .unwrap();
        let mut monitor = ChatLogMonitor::new(vec![root], Duration::from_secs(60));

        let initial = monitor.initialize();
        assert!(initial.events.is_empty());
        assert_eq!(initial.stats.active_files, 1);

        append(&path, "\u{feff}[ 2026.09.03 08:41:00 ] Scout > new\r\n");
        let polled = monitor.poll();

        assert_eq!(polled.events.len(), 1);
        assert_eq!(polled.events[0].message.text, "new");
        assert_eq!(polled.events[0].channel, "example.intel");
        assert_eq!(polled.events[0].character_id, 42);
    }

    #[test]
    fn a_newer_session_replaces_the_active_file_and_reads_its_messages() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("EVE/logs");
        let chatlogs = root.join("Chatlogs");
        fs::create_dir_all(&chatlogs).unwrap();
        let old = chatlogs.join(filename("example.intel", 42, 0));
        fs::write(&old, document("")).unwrap();
        let mut monitor = ChatLogMonitor::new(vec![root], Duration::from_secs(60));
        monitor.initialize();

        let new = chatlogs.join(filename("example.intel", 42, 60));
        fs::write(
            &new,
            document("\u{feff}[ 2026.09.03 09:36:22 ] Scout > first new message\r\n"),
        )
        .unwrap();
        let rescanned = monitor.rescan_now();
        assert_eq!(rescanned.stats.active_files, 1);

        let polled = monitor.poll();
        assert_eq!(polled.events.len(), 1);
        assert_eq!(polled.events[0].message.text, "first new message");
        assert_eq!(polled.events[0].source, new);
    }
}
