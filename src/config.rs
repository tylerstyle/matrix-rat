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
    /// Let others see how far you have read. When off, the server still learns it
    /// (so unread counts clear on your other devices) but tells nobody.
    pub read_receipts: bool,
    /// Where saved attachments go. Empty: the desktop's download folder.
    pub download_dir: String,
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
            read_receipts: true,
            download_dir: String::new(),
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

/// Changes one setting in the configuration file, keeping everything else in it
/// (comments included) as it is. The file is replaced in one step.
pub fn save_setting(path: &Path, key: &str, value: impl Into<toml_edit::Value>) -> Result<()> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("could not read {}", path.display())),
    };
    let mut document: toml_edit::DocumentMut = source
        .parse()
        .with_context(|| format!("invalid config {}", path.display()))?;
    let mut value: toml_edit::Value = value.into();
    // A comment after the old value stays where it is.
    if let Some(old) = document.get(key).and_then(|item| item.as_value()) {
        *value.decor_mut() = old.decor().clone();
    }
    document[key] = toml_edit::Item::Value(value);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("toml.new");
    std::fs::write(&temporary, document.to_string())
        .and_then(|()| std::fs::rename(&temporary, path))
        .with_context(|| format!("could not write {}", path.display()))
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
        assert!(config.read_receipts);
        assert_eq!(config.images, ImageMode::Auto);
    }

    #[test]
    fn saving_a_setting_keeps_the_rest_of_the_file() {
        let dir = std::env::temp_dir().join(format!("matrix-rat-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("config.toml");
        // No file yet: one is created.
        save_setting(&path, "mouse", false).unwrap();
        assert!(!Config::load(&path).unwrap().mouse);
        std::fs::write(
            &path,
            "# my colours\ntheme = \"terminal\" # follows kitty\nmouse = false\n",
        )
        .unwrap();
        save_setting(&path, "sidebar_width", 30).unwrap();
        save_setting(&path, "theme", "gruvbox-dark").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my colours"), "{text}");
        assert!(text.contains("# follows kitty"), "{text}");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.theme, "gruvbox-dark");
        assert_eq!(config.sidebar_width, 30);
        assert!(!config.mouse);
        // A broken file is left alone.
        std::fs::write(&path, "theme = [").unwrap();
        assert!(save_setting(&path, "mouse", true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theme = [");
        let _ = std::fs::remove_dir_all(&dir);
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
