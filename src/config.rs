use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_show_qr")]
    pub show_qr_on_start: bool,
}

fn default_base_url() -> String {
    "https://remux.sh".to_string()
}
fn default_show_qr() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            show_qr_on_start: default_show_qr(),
        }
    }
}

impl Config {
    pub fn load() -> Self {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    pub fn display_base_url(&self) -> &str {
        self.base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
    }
}

/// Returns the base directory for all remux data: `~/.remux`.
/// Override with `REMUX_HOME` env var.
pub fn remux_home() -> PathBuf {
    std::env::var("REMUX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".remux")
        })
}

fn config_path() -> PathBuf {
    remux_home().join("config.toml")
}
