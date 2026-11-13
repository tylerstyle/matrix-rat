//! Themes: a small palette plus semantic roles derived from it.
//!
//! A theme file only has to provide `[palette]`; every role has a default built from the
//! palette, and `[ui]` can override single roles. See `themes/*.toml`.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result, anyhow};
use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

const BUILTIN: &[(&str, &str)] = &[
    ("rat-dark", include_str!("../themes/rat-dark.toml")),
    (
        "catppuccin-mocha",
        include_str!("../themes/catppuccin-mocha.toml"),
    ),
    (
        "catppuccin-latte",
        include_str!("../themes/catppuccin-latte.toml"),
    ),
    ("gruvbox-dark", include_str!("../themes/gruvbox-dark.toml")),
    (
        "gruvbox-light",
        include_str!("../themes/gruvbox-light.toml"),
    ),
    ("tokyo-night", include_str!("../themes/tokyo-night.toml")),
    ("nord", include_str!("../themes/nord.toml")),
    ("dracula", include_str!("../themes/dracula.toml")),
    ("rose-pine", include_str!("../themes/rose-pine.toml")),
    (
        "solarized-light",
        include_str!("../themes/solarized-light.toml"),
    ),
    ("terminal", include_str!("../themes/terminal.toml")),
];

pub const DEFAULT_THEME: &str = "rat-dark";

/// Templates for theme generators, by the name used on the command line.
pub const TEMPLATES: &[(&str, &str)] = &[
    ("noctalia", include_str!("../contrib/themes/noctalia.toml")),
    ("matugen", include_str!("../contrib/themes/noctalia.toml")),
    ("wallust", include_str!("../contrib/themes/wallust.toml")),
    ("pywal", include_str!("../contrib/themes/pywal.toml")),
];

macro_rules! roles {
    ($($variant:ident => $name:literal),* $(,)?) => {
        /// A semantic slot the UI asks the theme for.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Role { $($variant),* }

        impl Role {
            pub const ALL: &'static [Role] = &[$(Role::$variant),*];

            fn from_name(name: &str) -> Option<Role> {
                match name { $($name => Some(Role::$variant),)* _ => None }
            }
        }
    };
}

roles! {
    Text => "text",
    Muted => "muted",
    Border => "border",
    BorderFocused => "border_focused",
    Title => "title",
    TitleFocused => "title_focused",
    SidebarSection => "sidebar_section",
    Room => "room",
    RoomUnread => "room_unread",
    RoomActive => "room_active",
    RoomCursor => "room_cursor",
    Hover => "hover",
    UnreadBadge => "unread_badge",
    MentionBadge => "mention_badge",
    Online => "online",
    Timestamp => "timestamp",
    DateDivider => "date_divider",
    MessageSelected => "message_selected",
    Reaction => "reaction",
    ReactionOwn => "reaction_own",
    ReplyQuote => "reply_quote",
    Placeholder => "placeholder",
    Banner => "banner",
    Status => "status",
    StatusKey => "status_key",
    StatusOk => "status_ok",
    Toast => "toast",
    Error => "error",
    OverlayBorder => "overlay_border",
    Scrollbar => "scrollbar",
    ScrollbarThumb => "scrollbar_thumb",
    Pill => "pill",
    Code => "code",
    Link => "link",
    Spoiler => "spoiler",
}

#[derive(Debug, Clone)]
struct Palette {
    bg: Color,
    surface: Color,
    surface_high: Color,
    fg: Color,
    muted: Color,
    outline: Color,
    primary: Color,
    on_primary: Color,
    secondary: Color,
    accent: Color,
    error: Color,
    success: Color,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    styles: Vec<Style>,
    users: Vec<Color>,
    /// Set when the terminal has no 24-bit colour.
    low_color: bool,
    /// Problems found while loading (unknown keys, bad colours). Never fatal.
    pub warnings: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ThemeFile {
    name: Option<String>,
    #[serde(default)]
    palette: BTreeMap<String, String>,
    #[serde(default)]
    ui: BTreeMap<String, StyleDef>,
    #[serde(default)]
    users: Users,
}

#[derive(Debug, Default, Deserialize)]
struct Users {
    #[serde(default)]
    colors: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StyleDef {
    Fg(String),
    Full {
        fg: Option<String>,
        bg: Option<String>,
        #[serde(default)]
        bold: bool,
        #[serde(default)]
        italic: bool,
        #[serde(default)]
        dim: bool,
        #[serde(default)]
        underline: bool,
        #[serde(default)]
        reversed: bool,
    },
}

impl Theme {
    pub fn style(&self, role: Role) -> Style {
        self.styles[role as usize]
    }

    /// A stable colour for a sender name.
    pub fn user_color(&self, name: &str) -> Color {
        let hash = name
            .bytes()
            .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
        self.users[hash as usize % self.users.len()]
    }

    pub fn builtin_names() -> Vec<String> {
        BUILTIN.iter().map(|(name, _)| (*name).to_owned()).collect()
    }

    pub fn builtin(name: &str) -> Option<Theme> {
        let (_, source) = BUILTIN.iter().find(|(n, _)| *n == name)?;
        Some(Self::parse(source, name).expect("built-in themes are valid"))
    }

    pub fn from_file(path: &Path) -> Result<Theme> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("could not read {}", path.display()))?;
        let fallback = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("custom");
        Self::parse(&source, fallback).with_context(|| format!("in {}", path.display()))
    }

    /// Looks in `themes_dir` first so users can override a built-in by name.
    pub fn load(name: &str, themes_dir: &Path) -> Result<Theme> {
        let file = themes_dir.join(format!("{name}.toml"));
        if file.exists() {
            return Self::from_file(&file);
        }
        Self::builtin(name).ok_or_else(|| anyhow!("unknown theme {name:?}"))
    }

    /// Built-in names plus `*.toml` files in `themes_dir`, without duplicates.
    pub fn available(themes_dir: &Path) -> Vec<String> {
        let mut names = Self::builtin_names();
        if let Ok(entries) = std::fs::read_dir(themes_dir) {
            let mut extra: Vec<String> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
                .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
                .filter(|n| !names.contains(n))
                .collect();
            extra.sort();
            names.extend(extra);
        }
        names
    }

    pub fn parse(source: &str, fallback_name: &str) -> Result<Theme> {
        let file: ThemeFile = toml::from_str(source).context("invalid theme file")?;
        let mut warnings = Vec::new();

        let mut color = |key: &str, default: Color| -> Color {
            match file.palette.get(key) {
                None => default,
                Some(value) => parse_color(value).unwrap_or_else(|| {
                    warnings.push(format!("palette.{key}: unknown colour {value:?}"));
                    default
                }),
            }
        };
        let bg = color("bg", Color::Reset);
        let fg = color("fg", Color::Reset);
        let surface = color("surface", bg);
        let muted = color("muted", Color::DarkGray);
        let primary = color("primary", Color::Blue);
        let palette = Palette {
            bg,
            fg,
            surface,
            muted,
            primary,
            surface_high: color("surface_high", surface),
            outline: color("outline", muted),
            on_primary: color(
                "on_primary",
                if bg == Color::Reset { Color::Black } else { bg },
            ),
            secondary: color("secondary", Color::Magenta),
            accent: color("accent", Color::Yellow),
            error: color("error", Color::Red),
            success: color("success", Color::Green),
        };
        // Accepted for forward compatibility, not used by a role yet.
        let _ = color("warning", Color::Yellow);
        const KNOWN: &[&str] = &[
            "bg",
            "fg",
            "surface",
            "surface_high",
            "muted",
            "outline",
            "primary",
            "on_primary",
            "secondary",
            "accent",
            "error",
            "success",
            "warning",
        ];
        for key in file.palette.keys().filter(|k| !KNOWN.contains(&k.as_str())) {
            warnings.push(format!("palette.{key}: unknown key"));
        }

        let lookup = |value: &str| -> Option<Color> {
            match value {
                "bg" => Some(palette.bg),
                "fg" => Some(palette.fg),
                "surface" => Some(palette.surface),
                "surface_high" => Some(palette.surface_high),
                "muted" => Some(palette.muted),
                "outline" => Some(palette.outline),
                "primary" => Some(palette.primary),
                "on_primary" => Some(palette.on_primary),
                "secondary" => Some(palette.secondary),
                "accent" => Some(palette.accent),
                "error" => Some(palette.error),
                "success" => Some(palette.success),
                other => parse_color(other),
            }
        };

        let mut styles: Vec<Style> = Role::ALL
            .iter()
            .map(|role| default_style(*role, &palette))
            .collect();
        for (key, def) in &file.ui {
            let Some(role) = Role::from_name(key) else {
                warnings.push(format!("ui.{key}: unknown role"));
                continue;
            };
            let mut resolve = |field: &str, value: &Option<String>| -> Option<Color> {
                let value = value.as_deref()?;
                let found = lookup(value);
                if found.is_none() {
                    warnings.push(format!("ui.{key}.{field}: unknown colour {value:?}"));
                }
                found
            };
            // An override replaces the role's default completely.
            let mut style = Style::new();
            match def {
                StyleDef::Fg(value) => {
                    if let Some(c) = resolve("fg", &Some(value.clone())) {
                        style = style.fg(c);
                    }
                }
                StyleDef::Full {
                    fg,
                    bg,
                    bold,
                    italic,
                    dim,
                    underline,
                    reversed,
                } => {
                    if let Some(c) = resolve("fg", fg) {
                        style = style.fg(c);
                    }
                    if let Some(c) = resolve("bg", bg) {
                        style = style.bg(c);
                    }
                    for (on, modifier) in [
                        (*bold, Modifier::BOLD),
                        (*italic, Modifier::ITALIC),
                        (*dim, Modifier::DIM),
                        (*underline, Modifier::UNDERLINED),
                        (*reversed, Modifier::REVERSED),
                    ] {
                        if on {
                            style = style.add_modifier(modifier);
                        }
                    }
                }
            }
            styles[role as usize] = style;
        }

        let mut users: Vec<Color> = Vec::new();
        for value in &file.users.colors {
            match lookup(value) {
                Some(c) => users.push(c),
                None => warnings.push(format!("users.colors: unknown colour {value:?}")),
            }
        }
        if users.is_empty() {
            users = vec![palette.primary, palette.secondary, palette.accent];
        }

        Ok(Theme {
            name: file.name.unwrap_or_else(|| fallback_name.to_owned()),
            styles,
            users,
            low_color: false,
            warnings,
        })
    }

    /// Replaces 24-bit colours with the nearest of the 256-colour palette, for terminals
    /// without truecolor support.
    /// A colour that comes from a message rather than from the theme.
    pub fn color(&self, r: u8, g: u8, b: u8) -> Color {
        if self.low_color {
            to_256(Color::Rgb(r, g, b))
        } else {
            Color::Rgb(r, g, b)
        }
    }

    pub fn downgrade_to_256(&mut self) {
        self.low_color = true;
        for style in &mut self.styles {
            style.fg = style.fg.map(to_256);
            style.bg = style.bg.map(to_256);
        }
        for color in &mut self.users {
            *color = to_256(*color);
        }
    }
}

fn default_style(role: Role, p: &Palette) -> Style {
    let s = Style::new();
    let bold = Modifier::BOLD;
    match role {
        Role::Text => s.fg(p.fg).bg(p.bg),
        Role::Muted | Role::Timestamp | Role::DateDivider | Role::Title => s.fg(p.muted),
        Role::Border | Role::Scrollbar => s.fg(p.outline),
        Role::BorderFocused | Role::OverlayBorder | Role::ScrollbarThumb => s.fg(p.primary),
        Role::TitleFocused => s.fg(p.primary).add_modifier(bold),
        Role::SidebarSection => s.fg(p.muted).add_modifier(bold),
        Role::Room => s.fg(p.fg),
        Role::RoomUnread => s.fg(p.fg).add_modifier(bold),
        Role::RoomActive => s.fg(p.primary).bg(p.surface_high).add_modifier(bold),
        Role::RoomCursor | Role::Hover => s.bg(p.surface),
        Role::UnreadBadge | Role::Toast | Role::Pill => {
            s.fg(p.on_primary).bg(p.primary).add_modifier(bold)
        }
        Role::MentionBadge => s.fg(p.on_primary).bg(p.error).add_modifier(bold),
        Role::Online | Role::StatusOk => s.fg(p.success),
        Role::MessageSelected => s.bg(p.surface_high),
        Role::Reaction => s.fg(p.fg).bg(p.surface),
        Role::ReactionOwn => s.fg(p.primary).bg(p.surface_high).add_modifier(bold),
        Role::ReplyQuote | Role::Placeholder => s.fg(p.muted).add_modifier(Modifier::ITALIC),
        Role::Banner => s.fg(p.secondary),
        Role::Status => s.fg(p.muted),
        Role::StatusKey => s.fg(p.accent).add_modifier(bold),
        Role::Error => s.fg(p.error),
        Role::Code => s.fg(p.accent),
        Role::Link => s.fg(p.secondary).add_modifier(Modifier::UNDERLINED),
        Role::Spoiler => s.fg(p.muted).bg(p.surface),
    }
}

/// `#rrggbb`, an ANSI colour name, a 0–255 index, or `reset`.
fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("reset") || value.eq_ignore_ascii_case("default") {
        return Some(Color::Reset);
    }
    Color::from_str(value).ok()
}

fn to_256(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    // Greys map onto the 24-step grey ramp, everything else onto the 6x6x6 cube.
    if r == g && g == b {
        return match r {
            0..=7 => Color::Indexed(16),
            248..=255 => Color::Indexed(231),
            _ => Color::Indexed(232 + ((r as u16 - 8) * 24 / 240) as u8),
        };
    }
    let step = |v: u8| -> u8 {
        match v {
            0..=47 => 0,
            48..=114 => 1,
            _ => (v - 35) / 40,
        }
    };
    Color::Indexed(16 + 36 * step(r) + 6 * step(g) + step(b))
}

/// True when the terminal advertises 24-bit colour.
pub fn truecolor_supported() -> bool {
    std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_load_without_warnings() {
        for name in Theme::builtin_names() {
            let theme = Theme::builtin(&name).unwrap();
            assert_eq!(theme.name, name);
            assert!(theme.warnings.is_empty(), "{name}: {:?}", theme.warnings);
        }
    }

    #[test]
    fn palette_only_theme_gets_all_roles() {
        let theme =
            Theme::parse("[palette]\nprimary = \"#ff0000\"\nbg = \"#000000\"", "x").unwrap();
        assert_eq!(theme.name, "x");
        assert_eq!(
            theme.style(Role::BorderFocused).fg,
            Some(Color::Rgb(255, 0, 0))
        );
        // on_primary defaults to bg so badges stay readable.
        assert_eq!(theme.style(Role::UnreadBadge).fg, Some(Color::Rgb(0, 0, 0)));
    }

    #[test]
    fn overrides_and_warnings() {
        let theme = Theme::parse(
            r##"
            [palette]
            primary = "#112233"
            nonsense = "#000000"
            accent = "not-a-colour"
            [ui]
            border = "primary"
            title = { fg = "#ffffff", bold = true }
            bogus_role = "red"
            "##,
            "x",
        )
        .unwrap();
        assert_eq!(
            theme.style(Role::Border).fg,
            Some(Color::Rgb(0x11, 0x22, 0x33))
        );
        assert!(
            theme
                .style(Role::Title)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(theme.warnings.len(), 3, "{:?}", theme.warnings);
    }

    #[test]
    fn templates_are_valid_themes_once_filled_in() {
        for (name, template) in TEMPLATES {
            // Stand in for the generator: replace every placeholder with a colour.
            let mut filled = String::new();
            let mut rest = *template;
            while let Some(start) = rest.find('{') {
                let close = if rest[start..].starts_with("{{") {
                    "}}"
                } else {
                    "}"
                };
                let end =
                    start + rest[start..].find(close).expect("unclosed placeholder") + close.len();
                filled.push_str(&rest[..start]);
                filled.push_str("#336699");
                rest = &rest[end..];
            }
            filled.push_str(rest);
            let theme = Theme::parse(&filled, name).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert!(theme.warnings.is_empty(), "{name}: {:?}", theme.warnings);
            assert_eq!(
                theme.style(Role::BorderFocused).fg,
                Some(Color::Rgb(0x33, 0x66, 0x99))
            );
        }
    }

    #[test]
    fn invalid_toml_is_an_error_not_a_panic() {
        assert!(Theme::parse("[palette", "x").is_err());
    }

    #[test]
    fn user_colors_are_stable() {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        assert_eq!(theme.user_color("alice"), theme.user_color("alice"));
    }

    #[test]
    fn downgrade_removes_rgb() {
        let mut theme = Theme::builtin(DEFAULT_THEME).unwrap();
        theme.downgrade_to_256();
        for role in Role::ALL {
            let style = theme.style(*role);
            assert!(!matches!(style.fg, Some(Color::Rgb(..))));
            assert!(!matches!(style.bg, Some(Color::Rgb(..))));
        }
        assert_eq!(to_256(Color::Rgb(255, 255, 255)), Color::Indexed(231));
        assert_eq!(to_256(Color::Rgb(255, 0, 0)), Color::Indexed(196));
    }
}
