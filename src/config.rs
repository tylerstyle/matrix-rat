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
    /// Let others see when you are typing.
    pub typing_notices: bool,
    /// How pictures are shown: "auto", "halfblocks" or "off".
    pub images: ImageMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageMode {
    /// Real graphics where the terminal supports them well, coloured blocks elsewhere.
    #[default]
    Auto,
    /// Always coloured blocks.
    Halfblocks,
    /// Only the file name.
    Off,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_owned(),
            sidebar_width: 24,
            mouse: true,
            typing_notices: true,
            images: ImageMode::Auto,
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
        assert_eq!(config.images, ImageMode::Auto);
    }

    #[test]
    fn image_modes() {
        for (text, mode) in [
            ("auto", ImageMode::Auto),
            ("halfblocks", ImageMode::Halfblocks),
            ("off", ImageMode::Off),
        ] {
            let config: Config = toml::from_str(&format!("images = \"{text}\"")).unwrap();
            assert_eq!(config.images, mode);
        }
        assert!(toml::from_str::<Config>("images = \"sixel\"").is_err());
    }
}
