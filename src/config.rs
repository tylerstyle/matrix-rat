//! User configuration (`~/.config/matrix-rat/config.toml`). Everything is optional.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::theme::DEFAULT_THEME;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name of a built-in theme or of a file in `themes/` (without `.toml`).
    pub theme: String,
    /// Width of the room list in columns.
    pub sidebar_width: u16,
    /// Capture the mouse. Turn off to use the terminal's own text selection.
    pub mouse: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_owned(),
            sidebar_width: 24,
            mouse: true,
        }
    }
}

impl Config {
    /// A missing file gives the defaults; a broken file is an error the user should see.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        toml::from_str(&source).with_context(|| format!("invalid config {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_keeps_defaults() {
        let config: Config = toml::from_str("theme = \"terminal\"").unwrap();
        assert_eq!(config.theme, "terminal");
        assert_eq!(config.sidebar_width, 24);
        assert!(config.mouse);
    }
}
