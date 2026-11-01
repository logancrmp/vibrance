//! Settings file and XDG paths (SPEC 3.4, 6.3).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::theme::{ColorMode, PaletteChoice};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TitleBar {
    #[default]
    Custom,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub palette: PaletteChoice,
    pub color_mode: ColorMode,
    pub title_bar: TitleBar,
    pub normalization: bool,
    pub type_to_jump_timeout_ms: u64,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            palette: PaletteChoice::default(),
            color_mode: ColorMode::default(),
            title_bar: TitleBar::default(),
            normalization: false,
            type_to_jump_timeout_ms: 1000,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Config {
        std::fs::read_to_string(paths.config_file())
            .ok()
            .and_then(|s| match toml::from_str(&s) {
                Ok(config) => Some(config),
                Err(err) => {
                    log::warn!("ignoring malformed config: {err}");
                    None
                }
            })
            .unwrap_or_default()
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
}

impl Paths {
    /// XDG locations, or everything under `$VIBRANCE_HOME` for tests and fixtures.
    pub fn resolve() -> Paths {
        if let Some(home) = std::env::var_os("VIBRANCE_HOME").map(PathBuf::from) {
            return Paths { config: home.join("config"), data: home.join("data"), cache: home.join("cache") };
        }
        let dirs = directories::ProjectDirs::from("", "", "vibrance");
        match dirs {
            Some(d) => Paths {
                config: d.config_dir().to_path_buf(),
                data: d.data_dir().to_path_buf(),
                cache: d.cache_dir().to_path_buf(),
            },
            None => {
                let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
                Paths {
                    config: home.join(".config/vibrance"),
                    data: home.join(".local/share/vibrance"),
                    cache: home.join(".cache/vibrance"),
                }
            }
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn database(&self) -> PathBuf {
        self.data.join("library.db")
    }

    pub fn create(&self) {
        for dir in [&self.config, &self.data, &self.cache] {
            if let Err(err) = std::fs::create_dir_all(dir) {
                log::error!("cannot create {}: {err}", dir.display());
            }
        }
    }
}
