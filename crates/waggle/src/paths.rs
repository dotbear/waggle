//! Where Waggle keeps its files.
//!
//! On Windows: settings in `%APPDATA%\Waggle`, everything else in
//! `%LOCALAPPDATA%\Waggle`. On Linux the XDG equivalents.

use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub home_dir: PathBuf,
    pub config_file: PathBuf,
    pub jump_bridges_file: PathBuf,
    pub data_dir: PathBuf,
    pub database_file: PathBuf,
    pub log_dir: PathBuf,
    pub lock_file: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let base = BaseDirs::new().context("could not find the home directory")?;
        let project =
            ProjectDirs::from("", "", "Waggle").context("could not find the app data folders")?;
        let config_dir = project.config_dir().to_path_buf();
        let data_dir = project.data_local_dir().to_path_buf();
        Ok(Self {
            home_dir: base.home_dir().to_path_buf(),
            config_file: config_dir.join("config.toml"),
            jump_bridges_file: config_dir.join("jump-bridges.toml"),
            database_file: data_dir.join("state.sqlite3"),
            log_dir: data_dir.join("logs"),
            lock_file: data_dir.join("waggle.lock"),
            data_dir,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.config_file.parent(),
            Some(self.data_dir.as_path()),
            Some(self.log_dir.as_path()),
        ]
        .into_iter()
        .flatten()
        {
            fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        Ok(())
    }
}
