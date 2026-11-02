//! Where EVE writes its logs on this machine.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crate::config::LogsConfig;

pub fn discover_roots(config: &LogsConfig, home_dir: &Path) -> Vec<PathBuf> {
    let candidates = if config.roots.is_empty() {
        default_log_candidates(home_dir)
    } else {
        config
            .roots
            .iter()
            .map(|path| expand_home(path, home_dir))
            .collect()
    };

    // Steam is reachable as both `~/.local/share/Steam` and the `~/.steam/steam`
    // symlink, and both are candidates. Resolved to one path before deduping,
    // or the same chat file is tailed twice and every report arrives in
    // duplicate.
    candidates
        .into_iter()
        .filter_map(normalize_log_root)
        .filter(|path| is_eve_logs_root(path))
        .map(|path| dunce::canonicalize(&path).unwrap_or(path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn default_log_candidates(home_dir: &Path) -> Vec<PathBuf> {
    let proton_suffix = "steamapps/compatdata/8500/pfx/drive_c/users/steamuser/Documents/EVE/logs";

    #[cfg(windows)]
    {
        // EVE writes to the Documents known folder, which OneDrive and group
        // policy can move away from the profile; the shell API knows where it
        // is now. The profile path stays as a fallback for an unusual setup.
        let mut candidates = Vec::new();
        if let Some(documents) =
            directories::UserDirs::new().and_then(|dirs| dirs.document_dir().map(Path::to_path_buf))
        {
            candidates.push(documents.join("EVE").join("logs"));
        }
        candidates.push(home_dir.join("Documents").join("EVE").join("logs"));
        let _ = proton_suffix;
        candidates
    }

    #[cfg(not(windows))]
    vec![
        home_dir.join("Documents/EVE/logs"),
        home_dir.join(".steam/steam").join(proton_suffix),
        home_dir.join(".local/share/Steam").join(proton_suffix),
        home_dir.join(
            ".var/app/com.eveonline.EveOnline/data/staging_prefix/drive_c/users/steamuser/Documents/EVE/logs",
        ),
    ]
}

/// Resolves a leading `~` in a configured path to the home directory.
pub fn expand_home(path: &Path, home_dir: &Path) -> PathBuf {
    if path == Path::new("~") {
        return home_dir.to_path_buf();
    }

    path.strip_prefix("~")
        .map(|suffix| home_dir.join(suffix))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_log_root(path: PathBuf) -> Option<PathBuf> {
    let is_child = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case("chatlogs") || name.eq_ignore_ascii_case("gamelogs")
        });

    if is_child {
        path.parent().map(Path::to_path_buf)
    } else {
        Some(path)
    }
}

fn is_eve_logs_root(path: &Path) -> bool {
    path.join("Chatlogs").is_dir() || path.join("Gamelogs").is_dir()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::tempdir;

    #[test]
    fn explicit_roots_override_installation_discovery() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("another-launcher/EVE/logs");
        fs::create_dir_all(root.join("Chatlogs")).unwrap();

        let config = LogsConfig {
            roots: vec![root.clone()],
        };

        assert_eq!(
            discover_roots(&config, temp.path()),
            vec![dunce::canonicalize(&root).unwrap()]
        );
    }

    #[test]
    fn accepts_a_chatlogs_directory_as_an_explicit_root() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("EVE/logs");
        fs::create_dir_all(root.join("Chatlogs")).unwrap();

        let config = LogsConfig {
            roots: vec![root.join("Chatlogs")],
        };

        assert_eq!(
            discover_roots(&config, temp.path()),
            vec![dunce::canonicalize(&root).unwrap()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_same_directory_behind_a_symlink_is_one_root() {
        // Steam is reachable as both ~/.local/share/Steam and the ~/.steam/steam
        // symlink, and both are default candidates. Two roots for one directory
        // would tail every chat file twice.
        let temp = tempdir().unwrap();
        let real = temp.path().join("Steam/EVE/logs");
        fs::create_dir_all(real.join("Chatlogs")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("Steam"), temp.path().join("steam-link"))
            .unwrap();

        let config = LogsConfig {
            roots: vec![real.clone(), temp.path().join("steam-link/EVE/logs")],
        };

        assert_eq!(
            discover_roots(&config, temp.path()),
            vec![dunce::canonicalize(&real).unwrap()]
        );
    }

    #[test]
    fn expands_tilde_in_explicit_roots() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("custom/EVE/logs");
        fs::create_dir_all(root.join("Gamelogs")).unwrap();

        let config = LogsConfig {
            roots: vec![PathBuf::from("~/custom/EVE/logs")],
        };

        assert_eq!(
            discover_roots(&config, temp.path()),
            vec![dunce::canonicalize(&root).unwrap()]
        );
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_the_documents_known_folder_comes_first() {
        let home = Path::new(r"C:\Users\pilot");
        let candidates = default_log_candidates(home);
        assert!(
            candidates.iter().all(|path| path.ends_with(r"EVE\logs")),
            "{candidates:?}"
        );
        assert_eq!(
            candidates.last().unwrap(),
            &home.join(r"Documents\EVE\logs")
        );
    }
}
