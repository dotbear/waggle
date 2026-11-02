use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
};

use anyhow::{Context, Result};

use super::{
    filename::ChatLogFile,
    format::{ChatLogHeader, ChatMessage, decode_utf16le, parse_header, parse_message},
};

const HEADER_READ_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StartPosition {
    Beginning,
    End,
    /// Resume from a durable checkpoint. The offset is clamped to the file's
    /// current length so a truncated or replaced log restarts cleanly instead
    /// of seeking past the end.
    Offset(u64),
}

#[derive(Debug)]
pub struct TailBatch {
    pub messages: Vec<ChatMessage>,
    pub offset: u64,
    pub was_truncated: bool,
}

#[derive(Debug)]
pub struct Tailer {
    pub file: ChatLogFile,
    pub header: Option<ChatLogHeader>,
    offset: u64,
    pending_line: String,
}

impl Tailer {
    pub fn open(file: ChatLogFile, start: StartPosition) -> Result<Self> {
        let length = fs::metadata(&file.path)
            .with_context(|| format!("could not inspect {}", file.path.display()))?
            .len();
        let offset = match start {
            StartPosition::Beginning => 0,
            StartPosition::End => even_prefix_length(length),
            StartPosition::Offset(offset) => even_prefix_length(offset.min(length)),
        };
        let header = read_header(&file)?;

        Ok(Self {
            file,
            header,
            offset,
            pending_line: String::new(),
        })
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn read_new_messages(&mut self) -> Result<TailBatch> {
        let length = fs::metadata(&self.file.path)
            .with_context(|| format!("could not inspect {}", self.file.path.display()))?
            .len();
        let was_truncated = length < self.offset;
        if was_truncated {
            self.offset = 0;
            self.pending_line.clear();
            self.header = read_header(&self.file)?;
        } else if self.header.is_none() {
            self.header = read_header(&self.file)?;
        }

        // Checked before opening: most polls find nothing new, and a stat is
        // all it takes to say so.
        let available = length.saturating_sub(self.offset);
        let readable = even_prefix_length(available);
        if readable == 0 {
            return Ok(TailBatch {
                messages: Vec::new(),
                offset: self.offset,
                was_truncated,
            });
        }

        let mut source = File::open(&self.file.path)
            .with_context(|| format!("could not open {}", self.file.path.display()))?;
        source.seek(SeekFrom::Start(self.offset))?;

        let mut bytes = vec![0; readable as usize];
        source.read_exact(&mut bytes)?;
        self.offset += readable;

        let decoded = decode_utf16le(&bytes)?;
        let mut text = std::mem::take(&mut self.pending_line);
        text.push_str(&decoded);

        let ends_with_line_break = text.ends_with('\n');
        let mut lines = text.split('\n').collect::<Vec<_>>();
        if !ends_with_line_break {
            self.pending_line = lines.pop().unwrap_or_default().to_owned();
        }

        let messages = lines
            .into_iter()
            .filter_map(parse_message)
            .collect::<Vec<_>>();

        Ok(TailBatch {
            messages,
            offset: self.offset,
            was_truncated,
        })
    }
}

fn even_prefix_length(length: u64) -> u64 {
    length - (length % 2)
}

fn read_header(file: &ChatLogFile) -> Result<Option<ChatLogHeader>> {
    let source = File::open(&file.path)
        .with_context(|| format!("could not open {}", file.path.display()))?;
    let mut bytes = Vec::new();
    source
        .take(HEADER_READ_LIMIT)
        .read_to_end(&mut bytes)
        .with_context(|| format!("could not read header from {}", file.path.display()))?;
    bytes.truncate(even_prefix_length(bytes.len() as u64) as usize);

    if bytes.is_empty() {
        return Ok(None);
    }

    Ok(parse_header(&decode_utf16le(&bytes)?))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
    };

    use super::*;
    use tempfile::tempdir;

    fn utf16le(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    }

    fn header() -> &'static str {
        "\u{feff}\r\n\
         ---------------------------------------------------------------\r\n\
         Channel ID:      channel-42\r\n\
         Channel Name:    example.intel\r\n\
         Listener:        Example Pilot\r\n\
         Session started: 2026.09.03 08:36:21\r\n\
         ---------------------------------------------------------------\r\n"
    }

    fn append(path: &std::path::Path, bytes: &[u8]) {
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn starting_at_end_suppresses_existing_history() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("example.intel_20260903_083621_42.txt");
        fs::write(
            &path,
            utf16le(&format!(
                "{}\u{feff}[ 2026.09.03 08:40:00 ] Scout > old report\r\n",
                header()
            )),
        )
        .unwrap();
        let file = ChatLogFile::parse(&path).unwrap();
        let mut tailer = Tailer::open(file, StartPosition::End).unwrap();

        assert!(tailer.read_new_messages().unwrap().messages.is_empty());

        append(
            &path,
            &utf16le("\u{feff}[ 2026.09.03 08:41:00 ] Scout > new report\r\n"),
        );
        let batch = tailer.read_new_messages().unwrap();

        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].text, "new report");
        assert_eq!(
            tailer.header.as_ref().unwrap().channel_name,
            "example.intel"
        );
    }

    #[test]
    fn waits_for_a_complete_utf16_line() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("example.intel_20260903_083621_42.txt");
        fs::write(&path, utf16le(header())).unwrap();
        let file = ChatLogFile::parse(&path).unwrap();
        let mut tailer = Tailer::open(file, StartPosition::End).unwrap();
        let encoded = utf16le("\u{feff}[ 2026.09.03 08:41:00 ] Scout > complete later\r\n");
        let split = encoded.len() - 3;

        append(&path, &encoded[..split]);
        assert!(tailer.read_new_messages().unwrap().messages.is_empty());

        append(&path, &encoded[split..]);
        let batch = tailer.read_new_messages().unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].text, "complete later");
    }

    #[test]
    fn recovers_from_file_truncation() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("example.intel_20260903_083621_42.txt");
        fs::write(&path, utf16le(header())).unwrap();
        let file = ChatLogFile::parse(&path).unwrap();
        let mut tailer = Tailer::open(file, StartPosition::End).unwrap();

        fs::write(
            &path,
            utf16le("\u{feff}[ 2026.09.03 08:42:00 ] Scout > after truncate\r\n"),
        )
        .unwrap();
        let batch = tailer.read_new_messages().unwrap();

        assert!(batch.was_truncated);
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].text, "after truncate");
    }
}
