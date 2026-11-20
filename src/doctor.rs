//! "What did the app find here?": the terminal, pictures, the desktop's helpers, keys
//! and the config, as a short report for the screen and for pasting into a bug report.
//!
//! Nothing private goes in: no paths, no names of the user's own files, no account,
//! no server, no room. Values that come from the environment (`TERM`,
//! `TERM_PROGRAM`) are shortened and cleaned; everything else is our own wording.
//!
//! The report is made from [`Facts`]: in the app from what the running app knows
//! (`App::doctor_facts`), on the command line (`matrix-rat doctor`) by
//! [`command_line`], which reads the config and asks the terminal the way the app does
//! at start, but takes no lock, logs nobody in and leaves the message store alone.

use std::ffi::OsStr;
use std::path::Path;

use crate::config::{Config, ImageMode};
use crate::images::{Graphics, Protocol};
use crate::keymap::Keymap;
use crate::matrix::SecretStoreMode;
use crate::notify::NotifyMode;

/// How a line is to be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// As it should be, or simply a fact.
    Fine,
    /// Works, with a limit worth knowing.
    Note,
    /// Something will not work until this is seen to.
    Missing,
}

impl Level {
    /// The mark in front of the line, on the screen and in the copied text.
    pub fn mark(self) -> char {
        match self {
            Level::Fine => ' ',
            Level::Note => '~',
            Level::Missing => '!',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub label: &'static str,
    pub value: String,
    /// The reason or what to do, when there is something to say.
    pub hint: Option<String>,
    pub level: Level,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub sections: Vec<Section>,
}

/// Everything the report is made from. Plain values, so that it can be tested without
/// a terminal or a desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub version: &'static str,
    pub term: String,
    pub term_program: String,
    /// Columns and rows; `None` where they are not known (not run in a terminal).
    pub size: Option<(u16, u16)>,
    pub truecolor: bool,
    pub no_color: bool,
    pub multiplexer: bool,
    pub ssh: bool,
    /// The terminal tells `Shift-Enter` from `Enter` (the kitty keyboard protocol);
    /// `None`: the terminal was not asked.
    pub keyboard_enhanced: Option<bool>,
    /// The terminal has said that its window went to the front or behind; `None` on
    /// the command line, where nothing listens for that.
    pub focus_reports: Option<bool>,
    pub mouse: bool,
    pub image_mode: ImageMode,
    /// How pictures are drawn; `None`: the terminal was not asked.
    pub graphics: Option<Graphics>,
    /// A terminal whose kitty placeholders are known not to work.
    pub placeholders_broken: bool,
    pub notifications: NotifyMode,
    pub do_not_disturb: bool,
    pub session_bus: bool,
    pub wayland: bool,
    pub x11: bool,
    pub wl_paste: bool,
    pub xclip: bool,
    /// `xdg-open`, which opens links, files and pictures in other programs.
    pub opener: bool,
    /// What would play a voice message in the app.
    pub audio: Audio,
    /// What would record a voice message.
    pub recording: Recording,
    /// `VISUAL` or `EDITOR` names an editor (never what it names).
    pub editor_set: bool,
    pub secret_store: SecretStoreMode,
    /// The theme in use comes from a file of the user's own (never named here).
    pub own_theme: bool,
    /// The key preset, and whether it is edited (`Keymap::preset_summary`).
    pub key_preset: String,
    pub key_problems: usize,
    /// Descriptions of the actions that lost the key their preset gives them (our own
    /// wording, see [`unbound`]).
    pub unbound: Vec<&'static str>,
    /// Our own setting names (see `Config::ignored`).
    pub ignored_settings: Vec<String>,
    /// The config file is not valid TOML at all.
    pub config_broken: bool,
}

/// What the `audio_player` setting comes to here. Never the command itself: it is the
/// user's own and can name a folder of theirs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audio {
    /// One of the players the app knows, found by itself or named in the setting.
    Known(crate::audio::Kind),
    /// A command of the user's own that is there to be run.
    Own,
    /// Looked for the players it knows and found none.
    NoneFound,
    /// The setting names a command that is not there or cannot be read as one.
    OwnMissing,
}

impl Audio {
    /// From the setting, with this process's `PATH`.
    pub fn found(setting: &str) -> Self {
        Self::from_choice(&crate::audio::choose_here(setting))
    }

    pub fn from_choice(choice: &Result<crate::audio::Player, crate::audio::NoPlayer>) -> Self {
        match choice {
            Ok(player) if player.kind() == crate::audio::Kind::Other => Audio::Own,
            Ok(player) => Audio::Known(player.kind()),
            Err(crate::audio::NoPlayer::NoneFound) => Audio::NoneFound,
            Err(_) => Audio::OwnMissing,
        }
    }
}

/// What the `audio_recorder` setting comes to here: the kinds of the two programs, or
/// what is missing. Never the command itself, as with [`Audio`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recording {
    /// A capture program the app knows, found by itself, and the encoder.
    Known(
        crate::audio::recorder::Capture,
        crate::audio::recorder::Encoder,
    ),
    /// A capture command of the user's own that is there to be run, and the encoder.
    Own(crate::audio::recorder::Encoder),
    /// A session over SSH (and the user named no recorder of their own): the far
    /// machine's microphone is not recorded from.
    OverSsh,
    /// No microphone was seen (and the user named no recorder of their own).
    NoMicrophone,
    /// None of the capture programs the app knows is there with its sound server.
    NoRecorder,
    /// There is a recorder, but nothing that encodes Opus.
    NoEncoder,
    /// The setting names a command that is not there or cannot be read as one.
    OwnMissing,
}

impl Recording {
    /// From the setting, with this process's `PATH` and what is seen of the sound here.
    pub fn found(setting: &str) -> Self {
        Self::from_choice(&crate::audio::recorder::choose_here(setting))
    }

    pub fn from_choice(
        choice: &Result<crate::audio::recorder::Recorder, crate::audio::recorder::NoRecorder>,
    ) -> Self {
        use crate::audio::recorder::{Capture, NoRecorder};
        match choice {
            Ok(recorder) if recorder.capture_kind() == Capture::Own => {
                Recording::Own(recorder.encoder_kind())
            }
            Ok(recorder) => Recording::Known(recorder.capture_kind(), recorder.encoder_kind()),
            Err(NoRecorder::OverSsh) => Recording::OverSsh,
            Err(NoRecorder::NoMicrophone) => Recording::NoMicrophone,
            Err(NoRecorder::NoCapture) => Recording::NoRecorder,
            Err(NoRecorder::NoEncoder) => Recording::NoEncoder,
            Err(NoRecorder::BadCommand | NoRecorder::Missing) => Recording::OwnMissing,
        }
    }
}

/// Is a program of this name in one of the folders of `path` (the `PATH` variable)?
/// Looks only; nothing is run.
pub fn on_path(name: &str, path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    std::env::split_paths(path).any(|dir| is_program(&dir.join(name)))
}

fn is_program(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The actions that have no key at all although their preset gives them one: the
/// config took it away, or gave it to another action. Those without a key by design
/// (reached through the palette and the menus) are not worth listing. Descriptions
/// from the keymap table, each once.
pub fn unbound(keymap: &Keymap) -> Vec<&'static str> {
    let preset = Keymap::preset_bindings(keymap.preset());
    let mut out: Vec<&'static str> = Vec::new();
    for spec in crate::keymap::ACTIONS {
        let meant = preset.iter().any(|binding| binding.action == spec.action);
        let bound = keymap
            .bindings()
            .iter()
            .any(|binding| binding.action == spec.action);
        if meant && !bound && !out.contains(&spec.description) {
            out.push(spec.description);
        }
    }
    out
}

/// The `secret_store` setting as it is written in the config.
fn secret_store_name(mode: SecretStoreMode) -> &'static str {
    match mode {
        SecretStoreMode::Auto => "auto",
        SecretStoreMode::Keyring => "keyring",
        SecretStoreMode::File => "file",
    }
}

/// A value from the environment, made fit to show: one line, no control characters,
/// short. Empty becomes "not set".
fn shown(value: &str) -> String {
    let clean = crate::text::sanitize_line(value);
    let clean = clean.trim();
    if clean.is_empty() {
        return "not set".to_owned();
    }
    let mut out: String = clean.chars().take(40).collect();
    if clean.chars().count() > 40 {
        out.push('…');
    }
    out
}

fn fine(label: &'static str, value: impl Into<String>) -> Line {
    Line {
        label,
        value: value.into(),
        hint: None,
        level: Level::Fine,
    }
}

fn with(label: &'static str, value: impl Into<String>, level: Level, hint: &str) -> Line {
    Line {
        label,
        value: value.into(),
        hint: Some(hint.to_owned()),
        level,
    }
}

fn yes_no(on: bool) -> &'static str {
    if on { "yes" } else { "no" }
}

/// What a line says when the terminal could not be asked.
const NOT_ASKED: &str =
    "the terminal is asked only when matrix-rat doctor runs in it, not into a pipe";

impl Report {
    pub fn new(facts: &Facts) -> Self {
        Self {
            sections: vec![
                terminal(facts),
                pictures(facts),
                desktop(facts),
                keys_and_config(facts),
            ],
        }
    }

    /// The report as plain text, for the clipboard and the command line.
    pub fn as_text(&self) -> String {
        let mut out = String::new();
        for section in &self.sections {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(section.title);
            out.push('\n');
            for line in &section.lines {
                out.push_str(&format!(
                    "{} {}: {}",
                    line.level.mark(),
                    line.label,
                    line.value
                ));
                if let Some(hint) = &line.hint {
                    out.push_str(&format!(" ({hint})"));
                }
                out.push('\n');
            }
        }
        out
    }

    /// How many lines say that something is missing.
    pub fn missing(&self) -> usize {
        self.sections
            .iter()
            .flat_map(|section| &section.lines)
            .filter(|line| line.level == Level::Missing)
            .count()
    }
}

fn terminal(facts: &Facts) -> Section {
    let mut lines = vec![
        fine("matrix-rat", facts.version),
        fine("TERM", shown(&facts.term)),
        fine("TERM_PROGRAM", shown(&facts.term_program)),
        match facts.size {
            Some((columns, rows)) => fine("Size", format!("{columns} x {rows}")),
            None => with("Size", "not known", Level::Fine, "not run in a terminal"),
        },
    ];
    lines.push(if facts.no_color {
        with(
            "Colours",
            "none",
            Level::Note,
            "NO_COLOR is set: bold, dim and reverse only",
        )
    } else if facts.truecolor {
        fine("Colours", "16 million")
    } else {
        with(
            "Colours",
            "256",
            Level::Note,
            "COLORTERM does not say truecolor; themes are approximated",
        )
    });
    lines.push(match facts.keyboard_enhanced {
        Some(true) => fine("Keyboard", "extended"),
        Some(false) => with(
            "Keyboard",
            "basic",
            Level::Note,
            "Shift-Enter cannot be told from Enter here; Alt-Enter makes a new line",
        ),
        None => with("Keyboard", "not asked", Level::Note, NOT_ASKED),
    });
    lines.push(if facts.mouse {
        fine("Mouse", "captured")
    } else {
        with(
            "Mouse",
            "off",
            Level::Note,
            "turned off in the settings; the terminal selects text itself",
        )
    });
    match facts.focus_reports {
        Some(true) => lines.push(fine("Window focus reports", "seen")),
        Some(false) => lines.push(with(
            "Window focus reports",
            "none yet",
            Level::Fine,
            "switch to another window and back to see; without them the window counts as \
             in front",
        )),
        None => {}
    }
    lines.push(fine("tmux, zellij or screen", yes_no(facts.multiplexer)));
    lines.push(fine("Over SSH", yes_no(facts.ssh)));
    Section {
        title: "Terminal",
        lines,
    }
}

fn pictures(facts: &Facts) -> Section {
    let setting = match facts.image_mode {
        ImageMode::Auto => "auto",
        ImageMode::Halfblocks => "halfblocks",
        ImageMode::Off => "off",
    };
    let mut lines = vec![fine("Setting", setting)];
    let Some(graphics) = facts.graphics else {
        lines.push(with("Shown as", "not asked", Level::Note, NOT_ASKED));
        return Section {
            title: "Pictures",
            lines,
        };
    };
    lines.push(match (facts.image_mode, graphics.protocol) {
        (ImageMode::Off, _) | (_, None) => with(
            "Shown as",
            "not shown",
            Level::Note,
            "pictures are turned off in the settings",
        ),
        (_, Some(Protocol::Kitty { .. })) => fine("Shown as", "real pictures (kitty graphics)"),
        (ImageMode::Halfblocks, Some(Protocol::Halfblocks)) => with(
            "Shown as",
            "coloured blocks",
            Level::Fine,
            "chosen in the settings",
        ),
        (ImageMode::Auto, Some(Protocol::Halfblocks)) => with(
            "Shown as",
            "coloured blocks",
            Level::Note,
            if facts.multiplexer {
                "inside tmux, zellij or screen real pictures are not reliable"
            } else if facts.placeholders_broken {
                "this terminal's support for kitty graphics is not reliable"
            } else {
                "the terminal did not offer kitty graphics; O opens a picture in the system's viewer"
            },
        ),
    });
    if graphics.protocol.is_some() {
        lines.push(fine(
            "Cell size",
            format!("{} x {} px", graphics.font.0, graphics.font.1),
        ));
    }
    Section {
        title: "Pictures",
        lines,
    }
}

fn desktop(facts: &Facts) -> Section {
    let mut lines = Vec::new();
    let resolved = match facts.notifications {
        NotifyMode::Auto if facts.session_bus && !facts.ssh => NotifyMode::Desktop,
        NotifyMode::Auto => NotifyMode::Terminal,
        other => other,
    };
    lines.push(match resolved {
        NotifyMode::Off => with(
            "Notifications",
            "off",
            Level::Note,
            "turned off in the settings",
        ),
        NotifyMode::Desktop if !facts.session_bus => with(
            "Notifications",
            "desktop service",
            Level::Missing,
            "no session bus found, so the desktop's service cannot be reached",
        ),
        NotifyMode::Desktop => fine("Notifications", "desktop service"),
        _ if facts.notifications == NotifyMode::Auto => with(
            "Notifications",
            "through the terminal",
            Level::Note,
            if facts.ssh {
                "over SSH the desktop's service is not used"
            } else {
                "no session bus found"
            },
        ),
        _ => fine("Notifications", "through the terminal"),
    });
    if facts.do_not_disturb {
        lines.push(with(
            "Do not disturb",
            "on",
            Level::Note,
            "nothing is announced until it is turned off",
        ));
    }
    lines.push(
        match (facts.wayland, facts.x11, facts.wl_paste, facts.xclip) {
            (true, _, true, _) => fine("Pasting a picture", "wl-paste"),
            (_, true, _, true) => fine("Pasting a picture", "xclip"),
            (true, _, false, _) => with(
                "Pasting a picture",
                "not possible",
                Level::Missing,
                "install wl-clipboard",
            ),
            (false, true, _, false) => with(
                "Pasting a picture",
                "not possible",
                Level::Missing,
                "install xclip",
            ),
            (false, false, _, _) => with(
                "Pasting a picture",
                "not possible",
                Level::Note,
                "no desktop session here",
            ),
        },
    );
    lines.push(if facts.opener {
        fine("Opening files and links", "xdg-open")
    } else {
        with(
            "Opening files and links",
            "not possible",
            Level::Missing,
            "xdg-open was not found (the package xdg-utils)",
        )
    });
    lines.push(match facts.audio {
        _ if facts.ssh => with(
            "Playing voice messages",
            "not offered",
            Level::Note,
            "over SSH the sound would play on the far machine; the file can be saved",
        ),
        Audio::Known(crate::audio::Kind::Mpv) => fine("Playing voice messages", "mpv"),
        Audio::Known(kind) => with(
            "Playing voice messages",
            kind.program(),
            Level::Note,
            "plays and stops; pause and seek need mpv",
        ),
        Audio::Own => with(
            "Playing voice messages",
            "a command of your own",
            Level::Note,
            "plays and stops; pause and seek need mpv",
        ),
        Audio::NoneFound => with(
            "Playing voice messages",
            "not possible",
            Level::Missing,
            "none of mpv, ffplay, pw-play, paplay, aplay was found",
        ),
        Audio::OwnMissing => with(
            "Playing voice messages",
            "not possible",
            Level::Missing,
            "the command named by audio_player was not found",
        ),
    });
    lines.push(match facts.recording {
        Recording::Known(capture, encoder) => fine(
            "Recording voice messages",
            format!("{} and {}", capture.program(), encoder.program()),
        ),
        Recording::Own(encoder) => fine(
            "Recording voice messages",
            format!("a command of your own and {}", encoder.program()),
        ),
        Recording::OverSsh => with(
            "Recording voice messages",
            "not offered",
            Level::Note,
            "over SSH the far machine's microphone would record; audio_recorder can name a recorder all the same",
        ),
        Recording::NoMicrophone => with(
            "Recording voice messages",
            "not offered",
            Level::Note,
            "no microphone was seen; if there is one, name a recorder in audio_recorder",
        ),
        Recording::NoRecorder => with(
            "Recording voice messages",
            "not possible",
            Level::Missing,
            "none of parec, parecord, pw-record, ffmpeg, arecord was found with its sound server",
        ),
        Recording::NoEncoder => with(
            "Recording voice messages",
            "not possible",
            Level::Missing,
            "no encoder: neither opusenc (the package opus-tools) nor ffmpeg was found",
        ),
        Recording::OwnMissing => with(
            "Recording voice messages",
            "not possible",
            Level::Missing,
            "the command named by audio_recorder was not found",
        ),
    });
    lines.push(if facts.editor_set {
        fine("Writing in an editor", "VISUAL or EDITOR names one")
    } else {
        with(
            "Writing in an editor",
            "not possible",
            Level::Note,
            "neither VISUAL nor EDITOR names an editor",
        )
    });
    lines.push(match facts.secret_store {
        SecretStoreMode::File => fine("Store key kept", "in a file (secret_store = file)"),
        SecretStoreMode::Keyring => with(
            "Store key kept",
            "in the system keyring (secret_store = keyring)",
            Level::Fine,
            "without the desktop session (over SSH, on a console) the account cannot be opened",
        ),
        SecretStoreMode::Auto => with(
            "Store key kept",
            format!(
                "keyring or file (secret_store = {})",
                secret_store_name(facts.secret_store)
            ),
            Level::Fine,
            "the system keyring when it answers, else a file",
        ),
    });
    Section {
        title: "Desktop",
        lines,
    }
}

fn keys_and_config(facts: &Facts) -> Section {
    let mut lines = vec![
        fine(
            "Theme",
            if facts.own_theme {
                "a file of your own"
            } else {
                "built in"
            },
        ),
        fine("Key preset", facts.key_preset.clone()),
    ];
    // A file that is not TOML has no key bindings to count.
    if !facts.config_broken {
        lines.push(if facts.key_problems == 0 {
            fine("Key bindings in the config", "all understood")
        } else {
            with(
                "Key bindings in the config",
                format!("{} not understood", facts.key_problems),
                Level::Missing,
                "the log names them",
            )
        });
    }
    if !facts.unbound.is_empty() {
        lines.push(with(
            "Without a key",
            facts.unbound.join(", "),
            Level::Note,
            "the config took their keys; the command palette still has them",
        ));
    }
    lines.push(if facts.config_broken {
        with(
            "Settings in the config",
            "the file is not valid TOML",
            Level::Missing,
            "matrix-rat does not start with it until it is mended",
        )
    } else if facts.ignored_settings.is_empty() {
        fine("Settings in the config", "all understood")
    } else {
        with(
            "Settings in the config",
            format!("not understood: {}", facts.ignored_settings.join(", ")),
            Level::Missing,
            "they keep their defaults",
        )
    });
    Section {
        title: "Keys and config",
        lines,
    }
}

/// The doctor's popup: the report as it was when opened, and how far it is scrolled.
#[derive(Clone)]
pub struct Popup {
    pub report: Report,
    /// Rows scrolled down; the drawing keeps it within what there is.
    pub scroll: usize,
    /// Rows shown, as of the last draw (for PgUp and PgDn).
    pub page: usize,
}

/// Hand-written, like every popup's: the report quotes the environment's `TERM`.
impl std::fmt::Debug for Popup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Popup")
            .field("sections", &self.report.sections.len())
            .field("missing", &self.report.missing())
            .field("scroll", &self.scroll)
            .finish()
    }
}

impl Popup {
    pub fn new(facts: &Facts) -> Self {
        Self {
            report: Report::new(facts),
            scroll: 0,
            page: 0,
        }
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add_signed(lines);
    }
}

/// `matrix-rat doctor`: the report for the config in `config_file` and this terminal,
/// as text.
///
/// Reads the config (a file that is not TOML is reported, not an error) and, only when
/// standard input and output are both a terminal, asks the terminal what the app asks
/// it at start, with the same code and time limits (see
/// `terminal::ask_for_doctor`). Takes no lock, logs nobody in, writes nothing.
pub fn command_line(config_file: &Path) -> String {
    use std::io::IsTerminal;
    let (config, config_broken) = read_config(config_file);
    let in_terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let asked = in_terminal
        .then(|| crate::terminal::ask_for_doctor(config.images).ok())
        .flatten();
    let size = in_terminal
        .then(|| crossterm::terminal::size().ok())
        .flatten();
    Report::new(&command_line_facts(&config, config_broken, asked, size)).as_text()
}

/// The config as the app would read it at start, and whether it is not TOML at all
/// (then the defaults stand in). The reason is not kept: it quotes the file.
fn read_config(file: &Path) -> (Config, bool) {
    match Config::load(file) {
        Ok(config) => (config, false),
        Err(_) => (Config::default(), true),
    }
}

/// The facts for the command line: the environment, the config, and what the terminal
/// answered (`asked`: the keyboard protocol and pictures; `None` if it was not asked).
fn command_line_facts(
    config: &Config,
    config_broken: bool,
    asked: Option<(bool, Graphics)>,
    size: Option<(u16, u16)>,
) -> Facts {
    let (keymap, problems) = Keymap::from_config(config);
    let own_theme = [
        Some(config.theme.as_str()),
        config.theme_dark.as_deref(),
        config.theme_light.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|name| !name.is_empty())
    .any(|name| crate::theme::Theme::builtin(name).is_none());
    Facts {
        size,
        keyboard_enhanced: asked.map(|(keyboard, _)| keyboard),
        focus_reports: None,
        mouse: config.mouse,
        image_mode: config.images,
        graphics: asked.map(|(_, graphics)| graphics),
        notifications: config.notifications,
        do_not_disturb: config.do_not_disturb,
        secret_store: config.secret_store,
        own_theme,
        key_preset: keymap.preset_summary(),
        key_problems: problems.len(),
        unbound: unbound(&keymap),
        ignored_settings: config.ignored.clone(),
        config_broken,
        audio: Audio::found(&config.audio_player),
        recording: Recording::found(&config.audio_recorder),
        ..Facts::from_environment()
    }
}

impl Facts {
    /// What the process's environment says, with everything else as a plain default
    /// for the caller to fill in from what it knows.
    pub fn from_environment() -> Self {
        let text = |name: &str| std::env::var(name).unwrap_or_default();
        let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        let path = std::env::var_os("PATH");
        Self {
            version: env!("CARGO_PKG_VERSION"),
            term: text("TERM"),
            term_program: text("TERM_PROGRAM"),
            size: None,
            truecolor: crate::theme::truecolor_supported(),
            no_color: crate::theme::no_color_wanted(),
            multiplexer: crate::images::in_multiplexer(),
            ssh: crate::portal::in_ssh_session(),
            keyboard_enhanced: None,
            focus_reports: None,
            mouse: true,
            image_mode: ImageMode::Auto,
            graphics: None,
            placeholders_broken: crate::images::placeholders_broken(),
            notifications: NotifyMode::Auto,
            do_not_disturb: false,
            session_bus: crate::notify::session_bus(),
            wayland: set("WAYLAND_DISPLAY"),
            x11: set("DISPLAY"),
            wl_paste: on_path("wl-paste", path.as_deref()),
            xclip: on_path("xclip", path.as_deref()),
            opener: on_path("xdg-open", path.as_deref()),
            audio: Audio::NoneFound,
            recording: Recording::NoMicrophone,
            editor_set: crate::editor::command_from_env().is_ok(),
            secret_store: SecretStoreMode::File,
            own_theme: false,
            key_preset: String::new(),
            key_problems: 0,
            unbound: Vec::new(),
            ignored_settings: Vec::new(),
            config_broken: false,
        }
    }
}

/// Fixed facts for tests and snapshots: nothing of the machine they run on.
#[cfg(test)]
pub(crate) fn sample() -> Facts {
    Facts {
        version: "1.2.3",
        term: "xterm-kitty".to_owned(),
        term_program: String::new(),
        size: Some((120, 40)),
        truecolor: true,
        no_color: false,
        multiplexer: false,
        ssh: false,
        keyboard_enhanced: Some(true),
        focus_reports: Some(true),
        mouse: true,
        image_mode: ImageMode::Auto,
        graphics: Some(Graphics {
            protocol: Some(Protocol::Kitty { compress: true }),
            font: (9, 18),
        }),
        placeholders_broken: false,
        notifications: NotifyMode::Auto,
        do_not_disturb: false,
        session_bus: true,
        wayland: true,
        x11: false,
        wl_paste: true,
        xclip: false,
        opener: true,
        audio: Audio::Known(crate::audio::Kind::Mpv),
        recording: Recording::Known(
            crate::audio::recorder::Capture::PwRecord,
            crate::audio::recorder::Encoder::Opusenc,
        ),
        editor_set: true,
        secret_store: SecretStoreMode::File,
        own_theme: false,
        key_preset: "default".to_owned(),
        key_problems: 0,
        unbound: Vec::new(),
        ignored_settings: Vec::new(),
        config_broken: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_audio_line_says_what_plays_and_never_the_users_command() {
        let line = |facts: &Facts| {
            Report::new(facts)
                .sections
                .iter()
                .flat_map(|section| section.lines.clone())
                .find(|line| line.label == "Playing voice messages")
                .unwrap()
        };
        let mut facts = sample();
        assert_eq!(line(&facts).value, "mpv");
        assert_eq!(line(&facts).level, Level::Fine);
        facts.audio = Audio::Known(crate::audio::Kind::Aplay);
        assert_eq!(line(&facts).value, "aplay");
        assert!(line(&facts).hint.unwrap().contains("need mpv"));
        facts.audio = Audio::Own;
        assert_eq!(line(&facts).value, "a command of your own");
        facts.audio = Audio::NoneFound;
        assert_eq!(line(&facts).level, Level::Missing);
        facts.audio = Audio::OwnMissing;
        assert!(line(&facts).hint.unwrap().contains("audio_player"));
        // Over SSH nothing plays, whatever is installed.
        facts.audio = Audio::Known(crate::audio::Kind::Mpv);
        facts.ssh = true;
        assert_eq!(line(&facts).value, "not offered");
        // The choice is told apart without the command being kept.
        assert_eq!(
            Audio::from_choice(&crate::audio::choose("no-such-player-x", None, None)),
            Audio::OwnMissing
        );
        assert_eq!(
            Audio::from_choice(&crate::audio::choose("auto", None, None)),
            Audio::NoneFound
        );
    }

    #[test]
    fn the_recording_line_says_what_was_found_or_is_missing_and_never_the_users_command() {
        use crate::audio::recorder::{Capture, Encoder, Env, Seen, choose};
        let line = |facts: &Facts| {
            Report::new(facts)
                .sections
                .iter()
                .flat_map(|section| section.lines.clone())
                .find(|line| line.label == "Recording voice messages")
                .unwrap()
        };
        let mut facts = sample();
        assert_eq!(line(&facts).value, "pw-record and opusenc");
        assert_eq!(line(&facts).level, Level::Fine);
        facts.recording = Recording::Known(Capture::FfmpegAlsa, Encoder::Ffmpeg);
        assert_eq!(line(&facts).value, "ffmpeg and ffmpeg");
        facts.recording = Recording::Own(Encoder::Ffmpeg);
        assert_eq!(line(&facts).value, "a command of your own and ffmpeg");
        facts.recording = Recording::NoMicrophone;
        assert_eq!(line(&facts).value, "not offered");
        assert_eq!(line(&facts).level, Level::Note);
        assert!(line(&facts).hint.unwrap().contains("microphone"));
        facts.recording = Recording::NoRecorder;
        assert_eq!(line(&facts).level, Level::Missing);
        assert!(line(&facts).hint.unwrap().contains("pw-record"));
        facts.recording = Recording::NoEncoder;
        assert!(line(&facts).hint.unwrap().contains("opusenc"));
        facts.recording = Recording::OwnMissing;
        assert!(line(&facts).hint.unwrap().contains("audio_recorder"));

        // The choice is told apart without the command being kept.
        let bin = std::env::temp_dir().join(format!(
            "matrix-rat-doctor-recording-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["secret-recorder", "ffmpeg", "arecord"] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(bin.join(name), "").unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let microphone = Seen {
            alsa_capture: true,
            ..Seen::default()
        };
        let env = |seen: &Seen, ssh: bool| Env {
            path: Some(bin.clone().into_os_string()),
            home: None,
            seen: *seen,
            ssh,
        };
        let found = |setting: &str, seen: &Seen| {
            Recording::from_choice(&choose(setting, &env(seen, false)))
        };
        // Over SSH nothing is recorded by itself; a command of the user's own is.
        facts.recording = Recording::from_choice(&choose("auto", &env(&microphone, true)));
        assert_eq!(facts.recording, Recording::OverSsh);
        assert_eq!(line(&facts).value, "not offered");
        assert_eq!(line(&facts).level, Level::Note);
        assert!(line(&facts).hint.unwrap().contains("SSH"));
        assert_eq!(
            Recording::from_choice(&choose("secret-recorder", &env(&microphone, true))),
            Recording::Own(Encoder::Ffmpeg)
        );
        let own = found("secret-recorder --device /home/alice/mic", &Seen::default());
        assert_eq!(own, Recording::Own(Encoder::Ffmpeg));
        facts.recording = own;
        let shown = format!("{:?} {:?}", line(&facts), facts.recording);
        assert!(
            !shown.contains("secret") && !shown.contains("alice"),
            "{shown}"
        );
        assert_eq!(
            found("auto", &microphone),
            Recording::Known(Capture::Arecord, Encoder::Ffmpeg)
        );
        assert_eq!(found("auto", &Seen::default()), Recording::NoMicrophone);
        assert_eq!(
            found("no-such-recorder-x", &microphone),
            Recording::OwnMissing
        );
        assert_eq!(found("'open", &microphone), Recording::OwnMissing);
        // Without arecord, ffmpeg records from ALSA; without both, nothing does.
        std::fs::remove_file(bin.join("arecord")).unwrap();
        assert_eq!(
            found("auto", &microphone),
            Recording::Known(Capture::FfmpegAlsa, Encoder::Ffmpeg)
        );
        std::fs::remove_file(bin.join("ffmpeg")).unwrap();
        assert_eq!(found("auto", &microphone), Recording::NoRecorder);
        assert_eq!(found("secret-recorder", &microphone), Recording::NoEncoder);
        std::fs::remove_dir_all(&bin).unwrap();
    }

    fn kitty_desktop() -> Facts {
        sample()
    }

    fn line<'a>(report: &'a Report, label: &str) -> &'a Line {
        report
            .sections
            .iter()
            .flat_map(|section| &section.lines)
            .find(|line| line.label == label)
            .unwrap_or_else(|| panic!("no line {label}"))
    }

    fn has_line(report: &Report, label: &str) -> bool {
        report
            .sections
            .iter()
            .flat_map(|section| &section.lines)
            .any(|line| line.label == label)
    }

    #[test]
    fn a_well_equipped_desktop_has_nothing_missing() {
        let report = Report::new(&kitty_desktop());
        assert_eq!(report.missing(), 0, "{}", report.as_text());
        assert_eq!(
            line(&report, "Shown as").value,
            "real pictures (kitty graphics)"
        );
        assert_eq!(line(&report, "Notifications").value, "desktop service");
        assert_eq!(line(&report, "TERM_PROGRAM").value, "not set");
        let text = report.as_text();
        assert!(
            text.starts_with("Terminal\n  matrix-rat: 1.2.3\n"),
            "{text}"
        );
        assert!(text.contains("\n\nPictures\n"), "{text}");
    }

    #[test]
    fn blocks_say_why() {
        let mut facts = kitty_desktop();
        facts.graphics = Some(Graphics::halfblocks());
        let why = |facts: &Facts| line(&Report::new(facts), "Shown as").hint.clone().unwrap();
        assert!(why(&facts).contains("did not offer"));
        facts.placeholders_broken = true;
        assert!(why(&facts).contains("not reliable"));
        facts.multiplexer = true;
        assert!(why(&facts).contains("tmux"));
        facts.image_mode = ImageMode::Halfblocks;
        assert!(why(&facts).contains("settings"));
        facts.image_mode = ImageMode::Off;
        facts.graphics = Some(Graphics::off());
        let report = Report::new(&facts);
        assert_eq!(line(&report, "Shown as").value, "not shown");
        // No pictures, no cell size to speak of.
        assert!(!has_line(&report, "Cell size"));
    }

    #[test]
    fn what_is_missing_is_marked_and_says_what_to_install() {
        let mut facts = kitty_desktop();
        facts.wl_paste = false;
        facts.opener = false;
        facts.key_problems = 2;
        facts.ignored_settings = vec!["mouse".to_owned()];
        let report = Report::new(&facts);
        assert_eq!(report.missing(), 4, "{}", report.as_text());
        assert_eq!(
            line(&report, "Pasting a picture").hint.as_deref(),
            Some("install wl-clipboard")
        );
        assert!(
            report
                .as_text()
                .contains("! Settings in the config: not understood: mouse")
        );
        // X11 only, with its tool.
        facts.wayland = false;
        facts.x11 = true;
        facts.xclip = true;
        assert_eq!(
            line(&Report::new(&facts), "Pasting a picture").value,
            "xclip"
        );
    }

    #[test]
    fn a_config_that_is_not_toml_is_named_and_its_keys_are_not_counted() {
        let mut facts = kitty_desktop();
        facts.config_broken = true;
        let report = Report::new(&facts);
        assert_eq!(report.missing(), 1, "{}", report.as_text());
        assert_eq!(
            line(&report, "Settings in the config").value,
            "the file is not valid TOML"
        );
        assert!(!has_line(&report, "Key bindings in the config"));
    }

    #[test]
    fn over_ssh_notifications_go_through_the_terminal_and_say_so() {
        let mut facts = kitty_desktop();
        facts.ssh = true;
        let report = Report::new(&facts);
        let notifications = line(&report, "Notifications");
        assert_eq!(notifications.value, "through the terminal");
        assert!(notifications.hint.as_deref().unwrap().contains("SSH"));
        // Asked for by name without a bus: that cannot work.
        facts.ssh = false;
        facts.session_bus = false;
        facts.notifications = NotifyMode::Desktop;
        assert_eq!(
            line(&Report::new(&facts), "Notifications").level,
            Level::Missing
        );
    }

    #[test]
    fn what_the_terminal_was_not_asked_says_so() {
        let mut facts = kitty_desktop();
        facts.size = None;
        facts.keyboard_enhanced = None;
        facts.graphics = None;
        facts.focus_reports = None;
        let report = Report::new(&facts);
        assert_eq!(line(&report, "Keyboard").value, "not asked");
        assert_eq!(line(&report, "Shown as").value, "not asked");
        assert_eq!(line(&report, "Size").value, "not known");
        assert!(!has_line(&report, "Cell size"));
        assert!(!has_line(&report, "Window focus reports"));
        // Nothing is missing for that: it was only not looked at.
        assert_eq!(report.missing(), 0);
    }

    #[test]
    fn values_from_the_environment_are_cleaned_and_cut() {
        let mut facts = kitty_desktop();
        facts.term = format!("evil\x1b[31m{}", "x".repeat(100));
        let report = Report::new(&facts);
        let term = &line(&report, "TERM").value;
        assert!(
            !term.contains('\x1b') && term.chars().count() <= 41,
            "{term}"
        );
    }

    #[test]
    fn the_secret_store_is_named_by_its_setting() {
        let mut facts = kitty_desktop();
        for (mode, name) in [
            (SecretStoreMode::File, "secret_store = file"),
            (SecretStoreMode::Keyring, "secret_store = keyring"),
            (SecretStoreMode::Auto, "secret_store = auto"),
        ] {
            facts.secret_store = mode;
            let report = Report::new(&facts);
            let value = &line(&report, "Store key kept").value;
            assert!(value.contains(name), "{value}");
        }
    }

    #[test]
    fn actions_that_lost_their_key_are_listed_and_those_without_one_by_design_are_not() {
        let keymap = Keymap::default();
        assert_eq!(unbound(&keymap), Vec::<&str>::new());
        // `mark_all_read` has no key in the preset: not worth naming. `reply` lost its.
        let config =
            Config::parse("[keys.timeline]\nreply = \"none\"\n[keys.global]\nsearch = []\n")
                .unwrap();
        let (keymap, problems) = Keymap::from_config(&config);
        assert!(problems.is_empty(), "{problems:?}");
        let lost = unbound(&keymap);
        assert_eq!(lost, ["Find in this room", "Reply to selected message"]);
        let mut facts = kitty_desktop();
        facts.unbound = lost;
        let report = Report::new(&facts);
        let without = line(&report, "Without a key");
        assert_eq!(without.level, Level::Note);
        assert!(without.value.contains("Reply to selected message"));
    }

    /// What `matrix-rat doctor` prints for this config file, without asking the
    /// terminal (the tests may well run in one).
    fn printed(file: &Path) -> String {
        let (config, broken) = read_config(file);
        Report::new(&command_line_facts(&config, broken, None, None)).as_text()
    }

    #[test]
    fn the_command_line_reads_a_config_without_writing_anything() {
        let dir = std::env::temp_dir().join(format!("mr-doctor-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config").join("config.toml");
        // No file: the defaults, and nothing is made.
        let text = printed(&file);
        assert!(
            text.contains("  Settings in the config: all understood"),
            "{text}"
        );
        assert!(text.contains("  Theme: built in"), "{text}");
        assert!(text.contains("~ Keyboard: not asked"), "{text}");
        assert!(!dir.exists());
        // Values that are not understood, by name; a theme of one's own, unnamed.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "mouse = \"yes\"\ntheme = \"alice-own-theme\"\n").unwrap();
        let text = printed(&file);
        assert!(
            text.contains("! Settings in the config: not understood: mouse"),
            "{text}"
        );
        assert!(text.contains("Theme: a file of your own"), "{text}");
        assert!(!text.contains("alice"), "{text}");
        // A file that is not TOML at all is said to be so, without the reason (which
        // quotes the file and names its folder).
        std::fs::write(&file, "mouse = = 1").unwrap();
        let text = printed(&file);
        assert!(text.contains("the file is not valid TOML"), "{text}");
        assert!(!text.contains(&*dir.to_string_lossy()), "{text}");
        // Only the config is there: nothing was written beside it.
        assert_eq!(walk(&dir), vec![file]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn a_program_is_found_on_the_path_without_being_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mr-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tool = dir.join("some-tool");
        std::fs::write(&tool, "").unwrap();
        let path = std::env::join_paths([Path::new("/nonexistent"), dir.as_path()]).unwrap();
        // A file that may not be run is not a program.
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!on_path("some-tool", Some(&path)));
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(on_path("some-tool", Some(&path)));
        assert!(!on_path("other-tool", Some(&path)));
        assert!(!on_path("some-tool", None));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_popup_scrolls_and_its_debug_output_quotes_nothing() {
        let mut facts = kitty_desktop();
        facts.term = "private-terminal-name".to_owned();
        let mut popup = Popup::new(&facts);
        popup.scroll_by(-3);
        assert_eq!(popup.scroll, 0);
        popup.scroll_by(5);
        assert_eq!(popup.scroll, 5);
        let debug = format!("{popup:?}");
        assert!(!debug.contains("private"), "{debug}");
    }
}
