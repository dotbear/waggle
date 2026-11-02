//! Telling files apart across platforms.
//!
//! A game log is followed by identity, not by name: EVE can start a new file
//! under a name that existed before, and the mining meter must notice that the
//! bytes it already counted belong to a different file. On Unix the identity
//! is the device and inode numbers; on Windows it is the volume serial number
//! and file index, which mean the same thing.

use std::{
    fs::Metadata,
    io,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Which file a path currently names.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// The identity of the file at `path`, whose metadata is `meta`.
///
/// On Unix this reads `meta` and nothing else. On Windows the file index is
/// not part of `std`'s metadata, so the file is opened briefly with no access
/// rights, which never conflicts with EVE holding it open for writing.
#[cfg(unix)]
pub fn identity(_path: &Path, meta: &Metadata) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    Ok(FileIdentity {
        device: meta.dev(),
        inode: meta.ino(),
    })
}

#[cfg(windows)]
pub fn identity(path: &Path, _meta: &Metadata) -> io::Result<FileIdentity> {
    match file_id::get_low_res_file_id(path)? {
        file_id::FileId::LowRes {
            volume_serial_number,
            file_index,
        } => Ok(FileIdentity {
            device: u64::from(volume_serial_number),
            inode: file_index,
        }),
        other => Err(io::Error::other(format!(
            "unexpected file identity {other:?} for {}",
            path.display()
        ))),
    }
}

/// When the file was last written, in whole seconds since the Unix epoch.
///
/// The same value Unix reports as `st_mtime`, read portably. A time before the
/// epoch (a clock or filesystem gone wrong) reads as zero, which only ever
/// makes a file look old.
pub fn modified_seconds(meta: &Metadata) -> io::Result<i64> {
    let modified = meta.modified()?;
    Ok(seconds_since_epoch(modified))
}

fn seconds_since_epoch(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn a_replaced_file_has_a_new_identity_and_a_rewrite_does_not() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("log.txt");
        fs::write(&path, "first").unwrap();
        let first = identity(&path, &fs::metadata(&path).unwrap()).unwrap();

        fs::write(&path, "first, then more").unwrap();
        let rewritten = identity(&path, &fs::metadata(&path).unwrap()).unwrap();
        assert_eq!(first, rewritten);

        // Keep the old file alive so its inode cannot be reused for the new one.
        let moved = temp.path().join("old.txt");
        fs::rename(&path, &moved).unwrap();
        fs::write(&path, "second").unwrap();
        let replaced = identity(&path, &fs::metadata(&path).unwrap()).unwrap();
        assert_ne!(first, replaced);
    }

    #[test]
    fn modification_time_matches_the_clock() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("log.txt");
        fs::write(&path, "now").unwrap();
        let written = modified_seconds(&fs::metadata(&path).unwrap()).unwrap();
        let now = seconds_since_epoch(SystemTime::now());
        assert!((now - written).abs() <= 5, "{written} vs {now}");
    }

    #[cfg(unix)]
    #[test]
    fn on_unix_the_modification_time_is_st_mtime() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("log.txt");
        fs::write(&path, "x").unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(modified_seconds(&meta).unwrap(), meta.mtime());
    }
}
