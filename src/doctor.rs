//! "What did the app find here?": the terminal, pictures, the desktop's helpers, keys
//! and the config, as a short report for the screen and for pasting into a bug report.
//!
//! Nothing private goes in: no paths, no names of the user's own files, no account,
//! no server, no room. Values that come from the environment (`TERM`,
//! `TERM_PROGRAM`) are shortened and cleaned; everything else is our own wording.

use std::ffi::OsStr;
use std::path::Path;

use crate::config::ImageMode;
use crate::images::{Graphics, Protocol};
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
    pub size: (u16, u16),
    pub truecolor: bool,
    pub no_color: bool,
    pub multiplexer: bool,
    pub ssh: bool,
    /// The terminal tells `Shift-Enter` from `Enter` (the kitty keyboard protocol).
    pub keyboard_enhanced: bool,
    pub mouse: bool,
    pub image_mode: ImageMode,
    pub graphics: Graphics,
    /// A terminal whose kitty placeholders are known not to work.
    pub placeholders_broken: bool,
    pub notifications: NotifyMode,
    pub do_not_disturb: bool,
    pub session_bus: bool,
    pub wayland: bool,
    pub x11: bool,
    pub wl_paste: bool,
    pub xclip: bool,
    pub opener: bool,
    pub editor_set: bool,
    pub secret_store: &'static str,
    pub key_preset: &'static str,
    pub key_problems: usize,
    /// Descriptions of the actions that have no key at all (our own wording).
    pub unbound: Vec<&'static str>,
    /// Our own setting names (see `Config::ignored`).
    pub ignored_settings: Vec<String>,
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

    /// The report as plain text, for the clipboard.
    pub fn as_text(&self) -> String {
        let mut out = String::new();
        for section in &self.sections {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(section.title);
            out.push('\n');
            for line in &section.lines {
                let mark = match line.level {
                    Level::Fine => ' ',
                    Level::Note => '~',
                    Level::Missing => '!',
                };
                out.push_str(&format!("{mark} {}: {}", line.label, line.value));
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
        fine("Size", format!("{} x {}", facts.size.0, facts.size.1)),
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
    lines.push(if facts.keyboard_enhanced {
        fine("Keyboard", "extended")
    } else {
        with(
            "Keyboard",
            "basic",
            Level::Note,
            "Shift-Enter cannot be told from Enter here; Alt-Enter makes a new line",
        )
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
    lines.push(fine(
        "Inside tmux, zellij or screen",
        yes_no(facts.multiplexer),
    ));
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
    let line = match (facts.image_mode, facts.graphics.protocol) {
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
    };
    Section {
        title: "Pictures",
        lines: vec![
            fine("Setting", setting),
            line,
            fine(
                "Cell size",
                format!("{} x {} px", facts.graphics.font.0, facts.graphics.font.1),
            ),
        ],
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
    lines.push(if facts.editor_set {
        fine("Writing in an editor", "VISUAL or EDITOR is set")
    } else {
        with(
            "Writing in an editor",
            "not possible",
            Level::Note,
            "neither VISUAL nor EDITOR is set",
        )
    });
    lines.push(fine("Store key kept in", facts.secret_store));
    Section {
        title: "Desktop",
        lines,
    }
}

fn keys_and_config(facts: &Facts) -> Section {
    let mut lines = vec![fine("Key preset", facts.key_preset)];
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
    if !facts.unbound.is_empty() {
        lines.push(with(
            "Without a key",
            facts.unbound.join(", "),
            Level::Fine,
            "reachable through the palette",
        ));
    }
    lines.push(if facts.ignored_settings.is_empty() {
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

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn kitty_desktop() -> Facts {
        Facts {
            version: "1.2.3",
            term: "xterm-kitty".to_owned(),
            term_program: String::new(),
            size: (120, 40),
            truecolor: true,
            no_color: false,
            multiplexer: false,
            ssh: false,
            keyboard_enhanced: true,
            mouse: true,
            image_mode: ImageMode::Auto,
            graphics: Graphics {
                protocol: Some(Protocol::Kitty { compress: true }),
                font: (9, 18),
            },
            placeholders_broken: false,
            notifications: NotifyMode::Auto,
            do_not_disturb: false,
            session_bus: true,
            wayland: true,
            x11: false,
            wl_paste: true,
            xclip: false,
            opener: true,
            editor_set: true,
            secret_store: "file",
            key_preset: "default",
            key_problems: 0,
            unbound: Vec::new(),
            ignored_settings: Vec::new(),
        }
    }

    fn line<'a>(report: &'a Report, label: &str) -> &'a Line {
        report
            .sections
            .iter()
            .flat_map(|section| &section.lines)
            .find(|line| line.label == label)
            .unwrap_or_else(|| panic!("no line {label}"))
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
        facts.graphics = Graphics::halfblocks();
        let why = |facts: &Facts| line(&Report::new(facts), "Shown as").hint.clone().unwrap();
        assert!(why(&facts).contains("did not offer"));
        facts.placeholders_broken = true;
        assert!(why(&facts).contains("not reliable"));
        facts.multiplexer = true;
        assert!(why(&facts).contains("tmux"));
        facts.image_mode = ImageMode::Halfblocks;
        assert!(why(&facts).contains("settings"));
        facts.image_mode = ImageMode::Off;
        facts.graphics = Graphics::off();
        assert_eq!(line(&Report::new(&facts), "Shown as").value, "not shown");
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
}
