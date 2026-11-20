//! User configuration (`~/.config/matrix-rat/config.toml`). Everything is optional.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::theme::DEFAULT_THEME;

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Name of a built-in theme or of a file in `themes/` (without `.toml`).
    pub theme: String,
    /// Optional override for `theme` when the terminal is in dark mode.
    pub theme_dark: Option<String>,
    /// Optional override for `theme` when the terminal is in light mode.
    pub theme_light: Option<String>,
    /// Width of the room list in columns.
    pub sidebar_width: u16,
    /// The room list is put away (the "hide or show the room list" key).
    pub sidebar_hidden: bool,
    /// Capture the mouse. Turn off to use the terminal's own text selection.
    pub mouse: bool,
    /// `Esc` in the composer, with nothing left to cancel, closes the chat: no room is
    /// open then, and the room list has the focus. Off: `Esc` stops at the composer.
    pub esc_closes_chat: bool,
    /// While the room list is out of sight (put away, or the window is narrow and a
    /// chat is open): a line above the chat that leads back to the list and shows the
    /// rooms with new notifications. Off: the chat has that line too.
    pub rooms_bar: bool,
    /// Let others see when you are typing.
    pub typing_notices: bool,
    /// Let others see how far you have read. When off, the server still learns it
    /// (so unread counts clear on your other devices) but tells nobody.
    pub read_receipts: bool,
    /// No notifications for now (the "Do not disturb" switch).
    pub do_not_disturb: bool,
    /// The terminal's window title shows the unread count (`matrix-rat (3)`), and the
    /// title from before is put back on exit. Off: the title is not touched at all.
    pub window_title: bool,
    /// Show under messages who has read up to there.
    pub read_markers: bool,
    /// Notifications for new messages: "auto", "desktop", "terminal" or "off".
    pub notifications: crate::notify::NotifyMode,
    /// The program's icon in desktop notifications: "color", "outline" or "generic".
    pub notification_icon: crate::notify::AppIcon,
    /// Show the text of the message in notifications.
    pub notification_preview: bool,
    /// A shell command run when a desktop notification is clicked, to bring the
    /// terminal window to the front. Empty: the built-in ways (see `focus`).
    pub notification_click_command: String,
    /// Show the latest message under each room in the room list (two lines a room).
    pub room_previews: bool,
    /// Ask the homeserver for a preview of the first link of a message: "off",
    /// "unencrypted" (rooms without encryption only) or "all". The server fetches the
    /// link, so it learns what is linked.
    pub link_previews: LinkPreviews,
    /// Where saved attachments go. Empty: the desktop's download folder.
    pub download_dir: String,
    /// What plays voice and audio messages: "auto" (the first of mpv, ffplay, pw-play,
    /// paplay and aplay on `PATH`), or a command line to which the file is added.
    pub audio_player: String,
    /// What records voice messages: "auto" (the first capture program whose sound
    /// server is there), or a command line that writes raw sound to its output.
    pub audio_recorder: String,
    /// What turns a voice or audio message into text, on this machine: "auto"
    /// (`whisper-cli` on `PATH`, with `transcribe_model`), or a command line to which
    /// the sound's file is added and which writes the text to its standard output.
    pub transcribe_command: String,
    /// The model file `whisper-cli` is given. Empty: transcribing is not offered
    /// with "auto".
    pub transcribe_model: String,
    /// What translates a message: a command line that reads the text on its standard
    /// input and writes the translation to its standard output. Empty: translating
    /// is not offered. Whether it works on this machine or asks a service is up to
    /// the command.
    pub translate_command: String,
    /// The language to translate into, handed to `translate_command` as it is.
    pub translate_language: String,
    /// Keep transcripts and translations across restarts, in a file that holds them
    /// in plain form. Off: they last as long as the app runs.
    pub keep_notes: bool,
    /// A command line of the user's own that summarises the unread messages of a room
    /// ("Catch up"): the text on its standard input, the summary on its standard
    /// output. Empty: there is none, and catching up is not offered.
    pub summary_command: String,
    /// How pictures are shown: "auto", "halfblocks" or "off".
    pub images: ImageMode,
    /// Where the local store passphrase is kept: "auto", "keyring" or "file".
    pub secret_store: crate::matrix::SecretStoreMode,
    /// Optional key-binding preset (`"default"` or `"vim"`). Validated by `keymap`.
    pub preset: Option<toml::Value>,
    /// Key-binding overrides (`[keys]` / `[keys.<context>]`). Validated by `keymap`.
    pub keys: Option<toml::Value>,
    /// Order-preserving raw `[keys]` data parsed via `toml_edit` in `Config::parse`.
    #[serde(skip)]
    pub raw_keys: Option<RawKeyConfig>,
    /// Settings of the file whose value was not understood. They keep their defaults;
    /// the rest of the file counts. Names only: they are our own setting names.
    #[serde(skip)]
    pub ignored: Vec<String>,
}

/// Hand-written: the download folder is a path, the click command and the summary
/// command can hold one (or a key), and
/// theme names can be the names of the user's own files; for those only whether they
/// are set is shown.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("theme_dark", &self.theme_dark.is_some())
            .field("theme_light", &self.theme_light.is_some())
            .field("sidebar_width", &self.sidebar_width)
            .field("sidebar_hidden", &self.sidebar_hidden)
            .field("mouse", &self.mouse)
            .field("esc_closes_chat", &self.esc_closes_chat)
            .field("rooms_bar", &self.rooms_bar)
            .field("typing_notices", &self.typing_notices)
            .field("read_receipts", &self.read_receipts)
            .field("do_not_disturb", &self.do_not_disturb)
            .field("window_title", &self.window_title)
            .field("read_markers", &self.read_markers)
            .field("notifications", &self.notifications)
            .field("notification_icon", &self.notification_icon)
            .field("notification_preview", &self.notification_preview)
            .field(
                "notification_click_command",
                &!self.notification_click_command.is_empty(),
            )
            .field("room_previews", &self.room_previews)
            .field("link_previews", &self.link_previews)
            .field("download_dir", &!self.download_dir.is_empty())
            .field(
                "audio_player_auto",
                &crate::audio::is_auto(&self.audio_player),
            )
            .field(
                "audio_recorder_auto",
                &crate::audio::is_auto(&self.audio_recorder),
            )
            .field(
                "transcribe_command_auto",
                &crate::audio::is_auto(&self.transcribe_command),
            )
            .field("transcribe_model", &!self.transcribe_model.is_empty())
            .field("translate_command", &!self.translate_command.is_empty())
            .field("keep_notes", &self.keep_notes)
            .field("summary_command", &!self.summary_command.trim().is_empty())
            .field("images", &self.images)
            .field("secret_store", &self.secret_store)
            .field("preset", &self.preset.is_some())
            .field("keys", &self.keys.is_some())
            .field("ignored", &self.ignored)
            .finish_non_exhaustive()
    }
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

/// In which rooms the homeserver is asked to preview links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkPreviews {
    /// Never.
    #[default]
    Off,
    /// In rooms that are not encrypted (or not known to be none).
    Unencrypted,
    /// Also in encrypted rooms: the homeserver then learns about links that were
    /// only meant for the people in the room.
    All,
}

impl LinkPreviews {
    pub const ALL: [LinkPreviews; 3] = [Self::Off, Self::Unencrypted, Self::All];

    /// The value in the configuration file.
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Unencrypted => "unencrypted",
            Self::All => "all",
        }
    }

    /// What the settings menu shows.
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Unencrypted => "unencrypted rooms",
            Self::All => "all rooms",
        }
    }

    /// The next (or previous, going back) of the three, round.
    pub fn step(self, delta: isize) -> Self {
        let at = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0) as isize;
        Self::ALL[(at + delta).rem_euclid(Self::ALL.len() as isize) as usize]
    }

    /// Whether a room may be asked about. A room whose encryption is not known yet
    /// counts as encrypted (the caller says so): a link is not given away by guessing.
    pub fn allows(self, encrypted: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Unencrypted => !encrypted,
            Self::All => true,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME.to_owned(),
            theme_dark: None,
            theme_light: None,
            sidebar_width: 24,
            sidebar_hidden: false,
            mouse: true,
            esc_closes_chat: true,
            rooms_bar: true,
            typing_notices: true,
            read_receipts: true,
            do_not_disturb: false,
            window_title: true,
            read_markers: true,
            notifications: crate::notify::NotifyMode::Auto,
            notification_icon: crate::notify::AppIcon::Color,
            notification_preview: true,
            notification_click_command: String::new(),
            room_previews: true,
            link_previews: LinkPreviews::Off,
            download_dir: String::new(),
            audio_player: "auto".to_owned(),
            audio_recorder: "auto".to_owned(),
            transcribe_command: "auto".to_owned(),
            transcribe_model: String::new(),
            translate_command: String::new(),
            translate_language: crate::notes::DEFAULT_LANGUAGE.to_owned(),
            keep_notes: crate::notes::KEEP_BY_DEFAULT,
            summary_command: String::new(),
            images: ImageMode::Auto,
            secret_store: crate::matrix::SecretStoreMode::File,
            preset: None,
            keys: None,
            raw_keys: None,
            ignored: Vec::new(),
        }
    }
}

impl Config {
    /// A missing file gives the defaults; a file that is not TOML at all is an error the
    /// user should see. One setting with a value we do not understand is not: see `parse`.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        Self::parse(&source).with_context(|| format!("invalid config {}", path.display()))
    }

    /// Parses TOML text into `Config`, keeping declaration order inside `[keys]`.
    ///
    /// A setting whose value is not understood (`mouse = "yes"`, `images = "sixel"`) does
    /// not take the rest of the file with it: it keeps its default and its name is
    /// listed in `ignored`, for the app to say so.
    pub fn parse(source: &str) -> Result<Config> {
        let mut table: toml::Table = toml::from_str(source)?;
        let mut ignored = Vec::new();
        if toml::Value::Table(table.clone())
            .try_into::<Config>()
            .is_err()
        {
            // Each setting on its own tells which ones are at fault.
            let names: Vec<String> = table.keys().cloned().collect();
            for name in names {
                let Some(value) = table.get(&name) else {
                    continue;
                };
                let mut alone = toml::Table::new();
                alone.insert(name.clone(), value.clone());
                if toml::Value::Table(alone).try_into::<Config>().is_err() {
                    table.remove(&name);
                    ignored.push(name);
                }
            }
        }
        let mut config: Config = toml::Value::Table(table).try_into()?;
        let document: toml_edit::DocumentMut = source.parse()?;
        config.raw_keys = Some(extract_raw_keys(&document));
        config.ignored = ignored;
        Ok(config)
    }

    /// What to tell the user about `ignored`, if anything.
    pub fn ignored_notice(&self) -> Option<String> {
        match self.ignored.as_slice() {
            [] => None,
            [one] => Some(format!(
                "Config: the value of `{one}` is not understood; its default is used"
            )),
            many => Some(format!(
                "Config: the values of {} are not understood; their defaults are used",
                many.iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
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

impl Config {
    /// Effective theme name for `scheme`: prefers the mode-specific slot (`theme_dark` /
    /// `theme_light`) when set and non-empty, otherwise falls back to `theme`.
    #[cfg(test)]
    pub fn theme_for(&self, scheme: crate::theme::ColorScheme) -> &str {
        let slot = match scheme {
            crate::theme::ColorScheme::Dark => self.theme_dark.as_deref(),
            crate::theme::ColorScheme::Light => self.theme_light.as_deref(),
        };
        slot.map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.theme)
    }

    /// Normalized `(theme_dark, theme_light)` slots, treating blank strings as unset.
    pub fn paired_themes(&self) -> (Option<String>, Option<String>) {
        let norm = |s: &Option<String>| {
            s.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        (norm(&self.theme_dark), norm(&self.theme_light))
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

/// Removes one top-level setting from the configuration file if the file exists, keeping
/// everything else (comments included) intact.
pub fn remove_setting(path: &Path, key: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    edit_document(path, |document| {
        document.as_table_mut().remove(key);
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
    fn one_bad_value_does_not_take_the_rest_of_the_file_with_it() {
        let config = Config::parse(
            "mouse = \"yes\"\n\
             read_markers = false\n\
             images = \"sixel\"\n\
             sidebar_width = 31\n\
             [keys]\n\
             quit = \"ctrl-x\"\n",
        )
        .unwrap();
        // The two that are wrong keep their defaults and are named, by the alphabet.
        assert_eq!(config.ignored, ["images", "mouse"]);
        assert_eq!(config.mouse, Config::default().mouse);
        assert_eq!(config.images, ImageMode::Auto);
        // Everything else counts, the key bindings too.
        assert!(!config.read_markers);
        assert_eq!(config.sidebar_width, 31);
        assert!(
            config
                .raw_keys
                .is_some_and(|raw| !raw.root_entries.is_empty())
        );
        let notice = Config::parse("mouse = 3")
            .unwrap()
            .ignored_notice()
            .unwrap();
        assert!(
            notice.contains("`mouse`") && !notice.contains('3'),
            "{notice}"
        );
    }

    #[test]
    fn a_good_file_ignores_nothing_and_a_file_that_is_not_toml_is_still_an_error() {
        let config = Config::parse("mouse = false\nnot_a_setting = 1\n").unwrap();
        assert!(config.ignored.is_empty() && config.ignored_notice().is_none());
        assert!(!config.mouse);
        assert!(Config::parse("mouse = = false").is_err());
    }

    #[test]
    fn debug_output_names_no_folder_no_command_and_no_theme() {
        let config = Config::parse(
            "theme = \"alice-theme\"\n\
             theme_dark = \"alice-dark\"\n\
             theme_light = \"alice-light\"\n\
             download_dir = \"/home/alice/secret-downloads\"\n\
             notification_click_command = \"raise-window --title alice\"\n\
             audio_player = \"/home/alice/bin/play-quietly\"\n\
             audio_recorder = \"/home/alice/bin/record-softly --device alice\"\n\
             transcribe_command = \"/home/alice/bin/hear-it\"\n\
             transcribe_model = \"/home/alice/models/ggml-base.bin\"\n\
             translate_command = \"trans-tool --key=sesame\"\n\
             translate_language = \"Klingon\"\n",
        )
        .unwrap();
        let shown = format!("{config:?}");
        for private in [
            "alice",
            "/home",
            "raise-window",
            "play-quietly",
            "record-softly",
            "hear-it",
            "ggml",
            "sesame",
            "trans-tool",
            "Klingon",
        ] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
        // Whether they are set still shows.
        assert!(shown.contains("download_dir: true"), "{shown}");
        assert!(shown.contains("audio_recorder_auto: false"), "{shown}");
        assert_eq!(
            config.audio_recorder,
            "/home/alice/bin/record-softly --device alice"
        );
    }

    #[test]
    fn partial_config_keeps_defaults() {
        let config: Config = toml::from_str("theme = \"terminal\"").unwrap();
        assert_eq!(config.theme, "terminal");
        assert_eq!(config.audio_player, "auto");
        assert_eq!(config.audio_recorder, "auto");
        // Transcribing looks for whisper-cli but has no model; translating has no
        // command; notes are in no file.
        assert_eq!(config.transcribe_command, "auto");
        assert_eq!(config.transcribe_model, "");
        assert_eq!(config.translate_command, "");
        assert_eq!(config.translate_language, "en");
        assert!(!config.keep_notes);
        assert_eq!(config.sidebar_width, 24);
        assert!(config.mouse);
        assert!(config.read_receipts);
        assert!(config.read_markers);
        let config: Config = toml::from_str("read_markers = false").unwrap();
        assert!(!config.read_markers);
        assert!(config.esc_closes_chat);
        let config: Config = toml::from_str("esc_closes_chat = false").unwrap();
        assert!(!config.esc_closes_chat);
        assert!(config.read_receipts);
        assert!(config.window_title);
        let config: Config = toml::from_str("window_title = false").unwrap();
        assert!(!config.window_title);
        assert!(config.esc_closes_chat);
        assert!(config.rooms_bar);
        let config: Config = toml::from_str("rooms_bar = false").unwrap();
        assert!(!config.rooms_bar);
        assert!(config.window_title);
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

    #[test]
    fn link_previews_are_off_unless_asked_for() {
        assert_eq!(Config::default().link_previews, LinkPreviews::Off);
        assert_eq!(
            toml::from_str::<Config>("theme = \"x\"")
                .unwrap()
                .link_previews,
            LinkPreviews::Off
        );
        for mode in LinkPreviews::ALL {
            let config: Config =
                toml::from_str(&format!("link_previews = \"{}\"", mode.name())).unwrap();
            assert_eq!(config.link_previews, mode);
        }
        assert!(toml::from_str::<Config>("link_previews = \"yes\"").is_err());
        assert!(toml::from_str::<Config>("link_previews = true").is_err());
    }

    #[test]
    fn link_preview_modes_decide_by_encryption_and_step_round() {
        use LinkPreviews::{All, Off, Unencrypted};
        assert_eq!(
            [Off, Unencrypted, All].map(|mode| (mode.allows(false), mode.allows(true))),
            [(false, false), (true, false), (true, true)]
        );
        assert_eq!(Off.step(1), Unencrypted);
        assert_eq!(All.step(1), Off);
        assert_eq!(Off.step(-1), All);
    }

    #[test]
    fn secret_store_modes() {
        use crate::matrix::SecretStoreMode;
        assert_eq!(Config::default().secret_store, SecretStoreMode::File);
        for (text, mode) in [
            ("auto", SecretStoreMode::Auto),
            ("keyring", SecretStoreMode::Keyring),
            ("file", SecretStoreMode::File),
        ] {
            let config: Config = toml::from_str(&format!("secret_store = \"{text}\"")).unwrap();
            assert_eq!(config.secret_store, mode);
        }
        assert!(toml::from_str::<Config>("secret_store = \"tpm\"").is_err());
    }

    #[test]
    fn paired_themes_resolve_per_scheme_and_remove_cleanly() {
        use crate::theme::ColorScheme;

        let config: Config = toml::from_str(
            "theme = \"terminal\"\ntheme_dark = \"gruvbox-dark\"\ntheme_light = \"catppuccin-latte\"\n",
        )
        .unwrap();
        assert_eq!(config.theme_for(ColorScheme::Dark), "gruvbox-dark");
        assert_eq!(config.theme_for(ColorScheme::Light), "catppuccin-latte");

        // Unset or whitespace-only slots fall back to `theme`.
        let partial: Config = toml::from_str(
            "theme = \"noctalia\"\ntheme_light = \"solarized-light\"\ntheme_dark = \" \"\n",
        )
        .unwrap();
        assert_eq!(partial.theme_for(ColorScheme::Dark), "noctalia");
        assert_eq!(partial.theme_for(ColorScheme::Light), "solarized-light");
        assert_eq!(
            partial.paired_themes(),
            (None, Some("solarized-light".to_owned()))
        );

        let dir = std::env::temp_dir().join(format!("matrix-rat-theme-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        remove_setting(&path, "theme_dark").unwrap();
        assert!(!path.exists());
        std::fs::write(&path, "# keep\ntheme = \"rat-dark\"\n").unwrap();
        save_setting(&path, "theme_dark", "nord").unwrap();
        save_setting(&path, "theme_light", "gruvbox-light").unwrap();
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.theme_for(ColorScheme::Dark), "nord");
        assert_eq!(loaded.theme_for(ColorScheme::Light), "gruvbox-light");
        remove_setting(&path, "theme_dark").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep"), "{text}");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.theme_dark, None);
        assert_eq!(loaded.theme_for(ColorScheme::Dark), "rat-dark");
        assert_eq!(loaded.theme_for(ColorScheme::Light), "gruvbox-light");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
