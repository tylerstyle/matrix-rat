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
    /// Notifications for new messages: "auto", "desktop", "terminal" or "off".
    pub notifications: crate::notify::NotifyMode,
    /// Show the text of the message in notifications.
    pub notification_preview: bool,
    /// Show the latest message under each room in the room list (two lines a room).
    pub room_previews: bool,
    /// Where saved attachments go. Empty: the desktop's download folder.
    pub download_dir: String,
    /// How pictures are shown: "auto", "halfblocks" or "off".
    pub images: ImageMode,
    /// Optional key-binding preset (`"default"` or `"vim"`). Validated by `keymap`.
    pub preset: Option<toml::Value>,
    /// Key-binding overrides (`[keys]` / `[keys.<context>]`). Validated by `keymap`.
    pub keys: Option<toml::Value>,
    /// Order-preserving raw `[keys]` data parsed via `toml_edit` in `Config::parse`.
    #[serde(skip)]
    pub raw_keys: Option<RawKeyConfig>,
}

/// One raw `[keys]` value before `keymap` interprets action and key names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawKeyValue {
    Str(String),
    List(Vec<Result<String, String>>),
    Invalid(String),
}

/// Preserved-order raw representation of `preset` and `[keys]` from `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawKeyConfig {
    pub top_preset: Option<Result<String, String>>,
    pub keys_preset: Option<Result<String, String>>,
    pub root_entries: Vec<(String, RawKeyValue)>,
    pub sections: Vec<(String, Vec<(String, RawKeyValue)>)>,
    pub syntax_warnings: Vec<String>,
}

/// One key override edit to persist into `[keys]` inside `config.toml`.
pub struct KeyOverrideEdit<'a> {
    pub context: &'a str,
    pub default_context: &'a str,
    pub action: &'a str,
    /// `None` removes any override (falling back to the preset default).
    pub keys: Option<&'a [String]>,
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
            notifications: crate::notify::NotifyMode::Auto,
            notification_preview: true,
            room_previews: true,
            download_dir: String::new(),
            images: ImageMode::Auto,
            preset: None,
            keys: None,
            raw_keys: None,
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
        Self::parse(&source).with_context(|| format!("invalid config {}", path.display()))
    }

    /// Parses TOML text into `Config`, keeping declaration order inside `[keys]`.
    pub fn parse(source: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(source)?;
        let document: toml_edit::DocumentMut = source.parse()?;
        config.raw_keys = Some(extract_raw_keys(&document));
        Ok(config)
    }

    /// Returns order-preserved raw key configuration, falling back to `serde`-decoded values
    /// if `Config` was constructed directly via `toml::from_str`.
    pub fn raw_key_config(&self) -> RawKeyConfig {
        if let Some(raw) = &self.raw_keys {
            return raw.clone();
        }
        raw_from_serde(self.preset.as_ref(), self.keys.as_ref())
    }
}

fn toml_value_type(v: &toml::Value) -> &'static str {
    match v {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

fn edit_item_type(item: &toml_edit::Item) -> &'static str {
    match item {
        toml_edit::Item::None => "none",
        toml_edit::Item::Value(v) => match v {
            toml_edit::Value::String(_) => "string",
            toml_edit::Value::Integer(_) => "integer",
            toml_edit::Value::Float(_) => "float",
            toml_edit::Value::Boolean(_) => "boolean",
            toml_edit::Value::Datetime(_) => "datetime",
            toml_edit::Value::Array(_) => "array",
            toml_edit::Value::InlineTable(_) => "inline table",
        },
        toml_edit::Item::Table(_) | toml_edit::Item::ArrayOfTables(_) => "table",
    }
}

fn parse_edit_key_value(item: &toml_edit::Item) -> RawKeyValue {
    match item {
        toml_edit::Item::Value(toml_edit::Value::String(s)) => RawKeyValue::Str(s.value().clone()),
        toml_edit::Item::Value(toml_edit::Value::Array(arr)) => {
            let items = arr
                .iter()
                .map(|elem| match elem {
                    toml_edit::Value::String(s) => Ok(s.value().clone()),
                    other => Err(format!("expected key string, got {other}")),
                })
                .collect();
            RawKeyValue::List(items)
        }
        other => RawKeyValue::Invalid(format!(
            "expected key string or list of strings, got {}",
            edit_item_type(other)
        )),
    }
}

fn parse_serde_key_value(val: &toml::Value) -> RawKeyValue {
    match val {
        toml::Value::String(s) => RawKeyValue::Str(s.clone()),
        toml::Value::Array(arr) => {
            let items = arr
                .iter()
                .map(|elem| match elem {
                    toml::Value::String(s) => Ok(s.clone()),
                    other => Err(format!(
                        "expected key string, got {}",
                        toml_value_type(other)
                    )),
                })
                .collect();
            RawKeyValue::List(items)
        }
        other => RawKeyValue::Invalid(format!(
            "expected key string or list of strings, got {}",
            toml_value_type(other)
        )),
    }
}

fn extract_raw_keys(doc: &toml_edit::DocumentMut) -> RawKeyConfig {
    let mut raw = RawKeyConfig::default();
    if let Some(item) = doc.get("preset") {
        raw.top_preset = Some(match item.as_str() {
            Some(s) => Ok(s.to_owned()),
            None => Err(format!(
                "preset: expected string, got {}",
                edit_item_type(item)
            )),
        });
    }
    let Some(keys_item) = doc.get("keys") else {
        return raw;
    };
    let Some(keys_table) = keys_item.as_table_like() else {
        raw.syntax_warnings.push(format!(
            "[keys]: expected table, got {}",
            edit_item_type(keys_item)
        ));
        return raw;
    };
    for (key, item) in keys_table.iter() {
        if key == "preset" {
            raw.keys_preset = Some(match item.as_str() {
                Some(s) => Ok(s.to_owned()),
                None => Err(format!(
                    "[keys].preset: expected string, got {}",
                    edit_item_type(item)
                )),
            });
            continue;
        }
        if let Some(sub) = item.as_table_like() {
            let mut entries = Vec::new();
            for (action, val_item) in sub.iter() {
                entries.push((action.to_owned(), parse_edit_key_value(val_item)));
            }
            raw.sections.push((key.to_owned(), entries));
        } else {
            raw.root_entries
                .push((key.to_owned(), parse_edit_key_value(item)));
        }
    }
    raw
}

fn raw_from_serde(preset: Option<&toml::Value>, keys: Option<&toml::Value>) -> RawKeyConfig {
    let mut raw = RawKeyConfig::default();
    if let Some(val) = preset {
        raw.top_preset = Some(match val.as_str() {
            Some(s) => Ok(s.to_owned()),
            None => Err(format!(
                "preset: expected string, got {}",
                toml_value_type(val)
            )),
        });
    }
    let Some(keys_val) = keys else {
        return raw;
    };
    let Some(table) = keys_val.as_table() else {
        raw.syntax_warnings.push(format!(
            "[keys]: expected table, got {}",
            toml_value_type(keys_val)
        ));
        return raw;
    };
    for (key, val) in table {
        if key == "preset" {
            raw.keys_preset = Some(match val.as_str() {
                Some(s) => Ok(s.to_owned()),
                None => Err(format!(
                    "[keys].preset: expected string, got {}",
                    toml_value_type(val)
                )),
            });
            continue;
        }
        if let Some(sub) = val.as_table() {
            let mut entries = Vec::new();
            for (action, item_val) in sub {
                entries.push((action.to_owned(), parse_serde_key_value(item_val)));
            }
            raw.sections.push((key.to_owned(), entries));
        } else {
            raw.root_entries
                .push((key.to_owned(), parse_serde_key_value(val)));
        }
    }
    raw
}

fn edit_document(path: &Path, edit: impl FnOnce(&mut toml_edit::DocumentMut)) -> Result<()> {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("could not read {}", path.display())),
    };
    let mut document: toml_edit::DocumentMut = source
        .parse()
        .with_context(|| format!("invalid config {}", path.display()))?;
    edit(&mut document);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("toml.new");
    std::fs::write(&temporary, document.to_string())
        .and_then(|()| std::fs::rename(&temporary, path))
        .with_context(|| format!("could not write {}", path.display()))
}

fn set_value_preserving_decor(
    table: &mut dyn toml_edit::TableLike,
    key: &str,
    mut value: toml_edit::Value,
) {
    if let Some(old) = table.get(key).and_then(|item| item.as_value()) {
        *value.decor_mut() = old.decor().clone();
    }
    if let Some(slot) = table.get_mut(key) {
        *slot = toml_edit::Item::Value(value);
    } else {
        table.insert(key, toml_edit::Item::Value(value));
    }
}

fn encode_key_list(keys: &[String]) -> toml_edit::Value {
    match keys {
        [single] => toml_edit::Value::from(single.as_str()),
        many => {
            let mut array = toml_edit::Array::new();
            for k in many {
                array.push(k.as_str());
            }
            toml_edit::Value::Array(array)
        }
    }
}

/// Changes one setting in the configuration file, keeping everything else in it
/// (comments included) as it is. The file is replaced in one step.
pub fn save_setting(path: &Path, key: &str, value: impl Into<toml_edit::Value>) -> Result<()> {
    let value = value.into();
    edit_document(path, |document| {
        set_value_preserving_decor(document.as_table_mut(), key, value);
    })
}

/// Persists key-binding changes (`preset` and/or `[keys]` action overrides) into `config.toml`
/// via `toml_edit`, leaving unrelated keys and comments untouched.
pub fn save_key_changes(
    path: &Path,
    preset: Option<&str>,
    clear_all_keys: bool,
    changes: &[KeyOverrideEdit<'_>],
) -> Result<()> {
    edit_document(path, |document| {
        if clear_all_keys {
            document.remove("keys");
            document.remove("preset");
        }
        if let Some(preset_name) = preset {
            if let Some(keys_tbl) = document.get_mut("keys").and_then(|i| i.as_table_like_mut())
                && keys_tbl.contains_key("preset")
            {
                set_value_preserving_decor(keys_tbl, "preset", toml_edit::Value::from(preset_name));
            } else if preset_name == "default" && !document.contains_key("preset") {
                // Keep untouched default configs clean when switching back to default.
            } else {
                set_value_preserving_decor(
                    document.as_table_mut(),
                    "preset",
                    toml_edit::Value::from(preset_name),
                );
            }
        }
        for change in changes {
            match change.keys {
                None => {
                    if let Some(keys_tbl) =
                        document.get_mut("keys").and_then(|i| i.as_table_like_mut())
                    {
                        if let Some(ctx_tbl) = keys_tbl
                            .get_mut(change.context)
                            .and_then(|i| i.as_table_like_mut())
                        {
                            ctx_tbl.remove(change.action);
                            if ctx_tbl.is_empty() {
                                keys_tbl.remove(change.context);
                            }
                        }
                        if change.context == change.default_context {
                            keys_tbl.remove(change.action);
                        }
                    }
                    let remove_keys = document
                        .get("keys")
                        .and_then(|i| i.as_table_like())
                        .is_some_and(|t| t.is_empty());
                    if remove_keys {
                        document.remove("keys");
                    }
                }
                Some(keys) => {
                    let encoded = encode_key_list(keys);
                    let in_root = change.context == change.default_context
                        && document
                            .get("keys")
                            .and_then(|i| i.as_table_like())
                            .is_some_and(|t| {
                                t.contains_key(change.action)
                                    && !t
                                        .get(change.context)
                                        .and_then(|c| c.as_table_like())
                                        .is_some_and(|c| c.contains_key(change.action))
                            });
                    if in_root {
                        if let Some(keys_tbl) =
                            document.get_mut("keys").and_then(|i| i.as_table_like_mut())
                        {
                            set_value_preserving_decor(keys_tbl, change.action, encoded);
                        }
                    } else {
                        if !document.get("keys").is_some_and(|i| i.is_table_like()) {
                            let mut tbl = toml_edit::Table::new();
                            tbl.set_implicit(true);
                            document.insert("keys", toml_edit::Item::Table(tbl));
                        }
                        if let Some(keys_tbl) =
                            document.get_mut("keys").and_then(|i| i.as_table_like_mut())
                        {
                            if !keys_tbl
                                .get(change.context)
                                .is_some_and(|i| i.is_table_like())
                            {
                                keys_tbl.insert(
                                    change.context,
                                    toml_edit::Item::Table(toml_edit::Table::new()),
                                );
                            }
                            if let Some(ctx_tbl) = keys_tbl
                                .get_mut(change.context)
                                .and_then(|i| i.as_table_like_mut())
                            {
                                set_value_preserving_decor(ctx_tbl, change.action, encoded);
                            }
                        }
                    }
                }
            }
        }
    })
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
    fn key_config_preserves_order_and_comments_on_save() {
        let dir = std::env::temp_dir().join(format!("matrix-rat-keyconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "# top comment\ntheme = \"nord\"\n\n[keys.global]\n# settings override\nsettings = \"f5\" # keep comment\nhelp = \"f6\"\n\n[keys.timeline]\n# last one wins check\nzebra_first = \"a\"\nalpha_second = \"a\"\n",
        )
        .unwrap();

        let loaded = Config::load(&path).unwrap();
        let raw = loaded.raw_key_config();
        assert_eq!(raw.sections.len(), 2);
        assert_eq!(raw.sections[1].0, "timeline");
        // TOML declaration order (zebra_first before alpha_second), not alphabetical!
        assert_eq!(raw.sections[1].1[0].0, "zebra_first");
        assert_eq!(raw.sections[1].1[1].0, "alpha_second");

        // Update settings override and add a multi-key reply override; reset help.
        save_key_changes(
            &path,
            Some("vim"),
            false,
            &[
                KeyOverrideEdit {
                    context: "global",
                    default_context: "global",
                    action: "settings",
                    keys: Some(&["f9".to_owned()]),
                },
                KeyOverrideEdit {
                    context: "global",
                    default_context: "global",
                    action: "help",
                    keys: None,
                },
                KeyOverrideEdit {
                    context: "timeline",
                    default_context: "timeline",
                    action: "reply",
                    keys: Some(&["enter".to_owned(), "R".to_owned()]),
                },
            ],
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# top comment"), "{text}");
        assert!(text.contains("# keep comment"), "{text}");
        assert!(text.contains("settings = \"f9\" # keep comment"), "{text}");
        assert!(text.contains("preset = \"vim\""), "{text}");
        assert!(!text.contains("help = \"f6\""), "{text}");
        assert!(text.contains("reply = [\"enter\", \"R\"]"), "{text}");

        // Reset all removes preset and [keys] while preserving unrelated config and comments.
        save_key_changes(&path, None, true, &[]).unwrap();
        let reset = std::fs::read_to_string(&path).unwrap();
        assert!(reset.contains("# top comment"), "{reset}");
        assert!(reset.contains("theme = \"nord\""), "{reset}");
        assert!(!reset.contains("[keys"), "{reset}");
        assert!(!reset.contains("preset"), "{reset}");

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
