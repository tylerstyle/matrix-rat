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

use crate::storage::{Key, Listing, Storage};

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
    (
        "high-contrast",
        include_str!("../themes/high-contrast.toml"),
    ),
    ("terminal", include_str!("../themes/terminal.toml")),
];

pub const DEFAULT_THEME: &str = "rat-dark";

/// Templates for theme generators, by the name used on the command line.
#[cfg(any(not(target_family = "wasm"), test))]
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
    CodeText => "code_text",
    CodeKeyword => "code_keyword",
    CodeString => "code_string",
    CodeComment => "code_comment",
    CodeConstant => "code_constant",
    CodeType => "code_type",
    CodeFunction => "code_function",
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

#[derive(Clone)]
pub struct Theme {
    pub name: String,
    styles: Vec<Style>,
    users: Vec<Color>,
    /// Set when the terminal has no 24-bit colour.
    low_color: bool,
    /// Set when the user asked for no colours at all (`NO_COLOR`).
    no_color: bool,
    /// Problems found while loading (unknown keys, bad colours). Never fatal.
    pub warnings: Vec<String>,
}

/// Hand-written: a theme of the user's own is named after a file of theirs, and what
/// was found wrong in it quotes that file.
impl std::fmt::Debug for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Theme")
            .field("styles", &self.styles.len())
            .field("users", &self.users.len())
            .field("low_color", &self.low_color)
            .field("no_color", &self.no_color)
            .field("warnings", &self.warnings.len())
            .finish_non_exhaustive()
    }
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

    /// The user's theme `name`, as `storage` keeps it.
    fn from_storage(storage: &dyn Storage, name: &str) -> Result<Theme> {
        let key = Key::Theme(name.to_owned());
        let source = crate::storage::read_found(storage, &key)?;
        // As the file's name has it (`name` may hold a folder).
        let file = format!("{name}.toml");
        let fallback = Path::new(&file)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("custom");
        Self::parse(&source, fallback).with_context(|| format!("in {}", storage.describe(&key)))
    }

    /// Looks among the user's themes first so users can override a built-in by name.
    pub fn load(name: &str, storage: &dyn Storage) -> Result<Theme> {
        if storage.exists(&Key::Theme(name.to_owned())) {
            return Self::from_storage(storage, name);
        }
        Self::builtin(name).ok_or_else(|| anyhow!("unknown theme {name:?}"))
    }

    /// Built-in names plus the user's themes, without duplicates.
    pub fn available(storage: &dyn Storage) -> Vec<String> {
        let mut names = Self::builtin_names();
        if let Ok(found) = storage.list(Listing::Themes) {
            let mut extra: Vec<String> = found.into_iter().filter(|n| !names.contains(n)).collect();
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
            if key == "hover" {
                // Retired when mouse-over highlighting was removed; ignored silently so
                // existing user themes load without warnings.
                continue;
            }
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
            no_color: false,
            warnings,
        })
    }

    /// Replaces 24-bit colours with the nearest of the 256-colour palette, for terminals
    /// without truecolor support.
    /// A colour that comes from a message rather than from the theme.
    pub fn color(&self, r: u8, g: u8, b: u8) -> Color {
        if self.no_color {
            Color::Reset
        } else if self.low_color {
            to_256(Color::Rgb(r, g, b))
        } else {
            Color::Rgb(r, g, b)
        }
    }

    /// For `NO_COLOR`: every colour goes, and what a background said (this is selected,
    /// this is a badge) is said by reversing or underlining instead. Bold, italic and
    /// the like stay.
    pub fn strip_colors(&mut self) {
        self.no_color = true;
        let plain_bg = self.style(Role::Text).bg;
        for role in Role::ALL {
            let style = &mut self.styles[*role as usize];
            let marked = style.bg.is_some_and(|bg| bg != Color::Reset) && style.bg != plain_bg;
            if marked && !style.add_modifier.contains(Modifier::REVERSED) {
                *style = match role {
                    // Under the room-list cursor: a quiet mark.
                    Role::RoomCursor => style.add_modifier(Modifier::UNDERLINED),
                    Role::Text => *style,
                    Role::Muted
                    | Role::Border
                    | Role::BorderFocused
                    | Role::Title
                    | Role::TitleFocused
                    | Role::SidebarSection
                    | Role::Room
                    | Role::RoomUnread
                    | Role::RoomActive
                    | Role::UnreadBadge
                    | Role::MentionBadge
                    | Role::Online
                    | Role::Timestamp
                    | Role::DateDivider
                    | Role::MessageSelected
                    | Role::Reaction
                    | Role::ReactionOwn
                    | Role::ReplyQuote
                    | Role::Placeholder
                    | Role::Banner
                    | Role::Status
                    | Role::StatusKey
                    | Role::StatusOk
                    | Role::Toast
                    | Role::Error
                    | Role::OverlayBorder
                    | Role::Scrollbar
                    | Role::ScrollbarThumb
                    | Role::Pill
                    | Role::Code
                    | Role::CodeText
                    | Role::CodeKeyword
                    | Role::CodeString
                    | Role::CodeComment
                    | Role::CodeConstant
                    | Role::CodeType
                    | Role::CodeFunction
                    | Role::Link
                    | Role::Spoiler => style.add_modifier(Modifier::REVERSED),
                };
            }
            style.fg = None;
            style.bg = None;
        }
        self.users.fill(Color::Reset);
    }

    pub fn downgrade_to_256(&mut self) {
        self.low_color = true;
        let original = self.styles.clone();
        for style in &mut self.styles {
            style.fg = style.fg.map(to_256);
            style.bg = style.bg.map(to_256);
        }
        separate_256_backgrounds(&original, &mut self.styles);
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
        Role::RoomCursor => s.bg(p.surface),
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
        // Code blocks in a known language. Plain code steps back so the marks stand out.
        Role::CodeText => s.fg(p.fg),
        Role::CodeKeyword => s.fg(p.secondary),
        Role::CodeString => s.fg(p.success),
        Role::CodeComment => s.fg(p.muted).add_modifier(Modifier::ITALIC),
        Role::CodeConstant => s.fg(p.accent),
        Role::CodeType => s.fg(p.accent).add_modifier(Modifier::ITALIC),
        // A colour of its own, not bold text: in a shell command the command is
        // often the only thing there is to mark.
        Role::CodeFunction => s.fg(p.primary),
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

const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Background roles that must stay visually distinguishable after 256-colour
/// mapping, in hierarchy order: earlier roles settle before later ones avoid them.
const SEPARATE_BG: &[(Role, &[Role])] = &[
    (Role::RoomCursor, &[Role::Text]),
    (Role::Reaction, &[Role::Text]),
    (Role::Spoiler, &[Role::Text]),
    (Role::RoomActive, &[Role::Text, Role::RoomCursor]),
    (
        Role::MessageSelected,
        &[Role::Text, Role::RoomCursor, Role::Reaction, Role::Spoiler],
    ),
    (Role::ReactionOwn, &[Role::Text, Role::Reaction]),
];

/// The sRGB colour of an xterm 256-colour index in `16..=255`
/// (the 6x6x6 colour cube and the 24-step grey ramp).
fn palette_256_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let v = 8 + 10 * (index - 232);
        (v, v, v)
    } else {
        let n = usize::from(index.saturating_sub(16));
        (
            CUBE_LEVELS[n / 36],
            CUBE_LEVELS[(n / 6) % 6],
            CUBE_LEVELS[n % 6],
        )
    }
}

/// Squared weighted RGB ("redmean") distance between two sRGB colours.
fn redmean_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let r_mean = (u32::from(a.0) + u32::from(b.0)) / 2;
    let dr = u32::from(a.0.abs_diff(b.0));
    let dg = u32::from(a.1.abs_diff(b.1));
    let db = u32::from(a.2.abs_diff(b.2));
    (512 + r_mean) * dr * dr + 1024 * dg * dg + (767 - r_mean) * db * db
}

/// Relative Rec. 709 brightness, scaled to integers (`0..=2_550_000`).
fn luma((r, g, b): (u8, u8, u8)) -> u32 {
    2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b)
}

/// The nearest of the 240 fixed colours by [`redmean_distance`] (the lower index on a
/// tie). Only the cube corners around the colour and the greys near its weighted mean
/// can be nearest: a search over these few gives the same index as one over all 240
/// for every one of the 16.7 million colours (checked once; a test checks a grid). It
/// matters: a picture drawn in half blocks asks for two colours a cell on every frame.
fn nearest_256(rgb: (u8, u8, u8)) -> u8 {
    // The one or two cube levels around a channel's value.
    let around = |v: u8| {
        let above = CUBE_LEVELS
            .iter()
            .position(|&level| level >= v)
            .unwrap_or(CUBE_LEVELS.len() - 1);
        above.saturating_sub(1)..=above
    };
    let mut best = (u32::MAX, 16u8);
    let mut consider = |idx: u8| {
        best = best.min((redmean_distance(rgb, palette_256_rgb(idx)), idx));
    };
    for r in around(rgb.0) {
        for g in around(rgb.1) {
            for b in around(rgb.2) {
                consider((16 + 36 * r + 6 * g + b) as u8);
            }
        }
    }
    // The grey steps around the channels' mean, weighted 2:4:3 (redmean's weights at low
    // red; they shift to 3:4:2 at full red, which the four steps searched absorb).
    let mean = (2 * u32::from(rgb.0) + 4 * u32::from(rgb.1) + 3 * u32::from(rgb.2)) / 9;
    let step = (mean.saturating_sub(8) / 10).min(23) as u8;
    for k in step.saturating_sub(1)..=(step + 2).min(23) {
        consider(232 + k);
    }
    best.1
}

/// [`nearest_256`] the slow way, over all 240 colours: what it must agree with.
#[cfg(test)]
fn nearest_256_of_all(rgb: (u8, u8, u8)) -> u8 {
    (16u8..=255)
        .min_by_key(|&idx| (redmean_distance(rgb, palette_256_rgb(idx)), idx))
        .unwrap_or(16)
}

/// Nearest colour over both the 6x6x6 cube (`16..=231`) and the grey ramp (`232..=255`).
fn to_256(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    Color::Indexed(nearest_256((r, g, b)))
}

/// Keeps background pairs that differed in 24-bit colour from collapsing onto the
/// same 256-colour index, nudging a collapsed role to the nearest distinct index
/// while preserving its relative lightness direction where possible.
fn separate_256_backgrounds(original: &[Style], styles: &mut [Style]) {
    for &(role, against) in SEPARATE_BG {
        let Some(Color::Rgb(r, g, b)) = original[role as usize].bg else {
            continue;
        };
        let Some(Color::Indexed(cur_idx)) = styles[role as usize].bg else {
            continue;
        };
        let orig_rgb = (r, g, b);
        let orig_luma = luma(orig_rgb);

        let mut avoid: Vec<u8> = Vec::new();
        let mut directions: Vec<(u32, bool)> = Vec::new();
        let mut collided = false;
        let mut collided_dir: Option<(u32, bool)> = None;

        for &other in against {
            let orig_other = original[other as usize].bg;
            if orig_other == Some(Color::Rgb(r, g, b)) {
                continue;
            }
            let Some(Color::Indexed(other_idx)) = styles[other as usize].bg else {
                continue;
            };
            avoid.push(other_idx);
            let dir = if other_idx >= 16 {
                let other_rgb = if let Some(Color::Rgb(or, og, ob)) = orig_other {
                    Some((or, og, ob))
                } else if let Some(Color::Indexed(i)) = orig_other
                    && i >= 16
                {
                    Some(palette_256_rgb(i))
                } else {
                    None
                };
                other_rgb.and_then(|rgb| {
                    let other_luma = luma(rgb);
                    (orig_luma != other_luma)
                        .then_some((luma(palette_256_rgb(other_idx)), orig_luma > other_luma))
                })
            } else {
                None
            };
            if let Some(d) = dir {
                directions.push(d);
            }
            if cur_idx == other_idx {
                collided = true;
                collided_dir = dir;
            }
        }
        if !collided {
            continue;
        }

        // Never nudge a background onto the text drawn over it.
        for fg_role in [role, Role::Text] {
            if original[fg_role as usize].fg != Some(Color::Rgb(r, g, b))
                && let Some(Color::Indexed(fg_idx)) = styles[fg_role as usize].fg
            {
                avoid.push(fg_idx);
            }
        }

        let pick = |predicate: &dyn Fn(u8) -> bool| -> Option<u8> {
            (16u8..=255)
                .filter(|idx| !avoid.contains(idx) && predicate(*idx))
                .min_by_key(|&idx| (redmean_distance(orig_rgb, palette_256_rgb(idx)), idx))
        };
        let fits = |idx: u8, (ref_luma, want_lighter): (u32, bool)| {
            let cand = luma(palette_256_rgb(idx));
            if want_lighter {
                cand > ref_luma
            } else {
                cand < ref_luma
            }
        };

        let chosen = pick(&|idx| directions.iter().all(|&d| fits(idx, d)))
            .or_else(|| collided_dir.and_then(|d| pick(&|idx| fits(idx, d))))
            .or_else(|| pick(&|_| true));
        if let Some(idx) = chosen {
            styles[role as usize].bg = Some(Color::Indexed(idx));
        }
    }
}

/// Whether the terminal currently uses a dark or light colour scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
}

/// True when the user asked programs not to use colour: `NO_COLOR` set to anything but
/// nothing (https://no-color.org).
#[cfg(any(not(target_family = "wasm"), test))]
pub fn no_color_wanted() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

/// True when the terminal advertises 24-bit colour.
#[cfg(any(not(target_family = "wasm"), test))]
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
            // Generators render comments too and fail on a placeholder they cannot
            // resolve (issue #1).
            for comment in template.lines().filter(|l| l.trim_start().starts_with('#')) {
                assert!(!comment.contains('{'), "{name}: {comment}");
            }
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
    fn without_colours_nothing_is_coloured_and_marks_stay_visible() {
        for name in Theme::builtin_names() {
            let mut theme = Theme::builtin(&name).unwrap();
            theme.strip_colors();
            for role in Role::ALL {
                let style = theme.style(*role);
                assert_eq!((style.fg, style.bg), (None, None), "{name} {role:?}");
            }
            assert_eq!(theme.user_color("alice"), Color::Reset);
            assert_eq!(theme.color(255, 0, 0), Color::Reset);
            // What was told apart by a background still is.
            for role in [
                Role::MessageSelected,
                Role::UnreadBadge,
                Role::Pill,
                Role::RoomActive,
            ] {
                assert!(
                    theme.style(role).add_modifier.contains(Modifier::REVERSED),
                    "{name} {role:?}"
                );
            }
            assert!(
                !theme
                    .style(Role::Text)
                    .add_modifier
                    .contains(Modifier::REVERSED)
            );
            assert!(
                !theme
                    .style(Role::RoomCursor)
                    .add_modifier
                    .contains(Modifier::REVERSED)
            );
        }
    }

    #[test]
    fn retired_hover_role_loads_without_warning() {
        let full = Theme::parse(
            r#"
[palette]
bg = "reset"
fg = "reset"

[ui]
hover = { bg = "surface", underline = true }
"#,
            "custom-full",
        )
        .unwrap();
        assert!(full.warnings.is_empty(), "{:?}", full.warnings);

        let short = Theme::parse(
            r#"
[palette]
bg = "reset"
fg = "reset"

[ui]
hover = "primary"
"#,
            "custom-short",
        )
        .unwrap();
        assert!(short.warnings.is_empty(), "{:?}", short.warnings);
    }

    /// The quick search agrees with the one over all 240 colours (checked once for
    /// every colour; here on a grid with the values where the nearest level changes).
    #[test]
    fn the_quick_nearest_colour_is_the_nearest_of_all() {
        let mut values: Vec<u8> = (0..=255).step_by(9).collect();
        values.extend([
            47, 48, 114, 115, 116, 154, 155, 156, 194, 195, 196, 234, 235, 255,
        ]);
        for &r in &values {
            for &g in &values {
                for &b in &values {
                    assert_eq!(
                        nearest_256((r, g, b)),
                        nearest_256_of_all((r, g, b)),
                        "{r} {g} {b}"
                    );
                }
            }
        }
    }

    #[test]
    fn to_256_maps_cube_grey_ramp_and_near_greys() {
        // Exact cube corners and interior points.
        assert_eq!(to_256(Color::Rgb(0, 0, 0)), Color::Indexed(16));
        assert_eq!(to_256(Color::Rgb(255, 255, 255)), Color::Indexed(231));
        assert_eq!(to_256(Color::Rgb(255, 0, 0)), Color::Indexed(196));
        assert_eq!(to_256(Color::Rgb(0, 255, 0)), Color::Indexed(46));
        assert_eq!(to_256(Color::Rgb(0, 0, 255)), Color::Indexed(21));
        assert_eq!(to_256(Color::Rgb(95, 95, 95)), Color::Indexed(59));
        assert_eq!(to_256(Color::Rgb(135, 175, 215)), Color::Indexed(110));

        // Exact grey ramp endpoints and steps (8 + 10*k).
        for k in 0..24u8 {
            let v = 8 + 10 * k;
            assert_eq!(to_256(Color::Rgb(v, v, v)), Color::Indexed(232 + k));
        }

        // Dark near-greys (rat-dark bg/surface/surface_high) land on the grey ramp
        // instead of collapsing onto cube black (16).
        for (rgb, expected) in [
            ((0x16, 0x16, 0x1d), 234),
            ((0x1f, 0x1f, 0x28), 235),
            ((0x2a, 0x2a, 0x37), 236),
        ] {
            let Color::Indexed(idx) = to_256(Color::Rgb(rgb.0, rgb.1, rgb.2)) else {
                panic!("expected indexed colour");
            };
            assert!((232..=255).contains(&idx), "{rgb:?} -> {idx}");
            assert_eq!(idx, expected, "{rgb:?}");
        }

        // Non-RGB colours pass through unchanged.
        assert_eq!(to_256(Color::Reset), Color::Reset);
        assert_eq!(to_256(Color::Blue), Color::Blue);
        assert_eq!(to_256(Color::Indexed(42)), Color::Indexed(42));
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

    #[test]
    fn every_builtin_keeps_distinct_roles_apart_in_256_colours() {
        for name in Theme::builtin_names() {
            let original = Theme::builtin(&name).unwrap();
            let mut downgraded = original.clone();
            downgraded.downgrade_to_256();

            for role in Role::ALL {
                let style = downgraded.style(*role);
                assert!(
                    !matches!(style.fg, Some(Color::Rgb(..))),
                    "{name} {role:?} fg"
                );
                assert!(
                    !matches!(style.bg, Some(Color::Rgb(..))),
                    "{name} {role:?} bg"
                );
            }
            for &color in &downgraded.users {
                assert!(!matches!(color, Color::Rgb(..)), "{name} user colour");
            }

            // Calling downgrade_to_256 twice is a no-op.
            let mut twice = downgraded.clone();
            twice.downgrade_to_256();
            assert_eq!(downgraded.styles, twice.styles, "{name} idempotent");

            if name == "terminal" {
                assert!(
                    downgraded
                        .style(Role::RoomCursor)
                        .add_modifier
                        .contains(Modifier::UNDERLINED)
                );
                assert!(
                    downgraded
                        .style(Role::RoomActive)
                        .add_modifier
                        .contains(Modifier::REVERSED)
                );
                continue;
            }

            let bg = |role: Role| downgraded.style(role).bg;
            let orig_bg = |role: Role| original.style(role).bg;
            for (a, b) in [
                (Role::RoomCursor, Role::Text),
                (Role::RoomActive, Role::Text),
                (Role::RoomCursor, Role::RoomActive),
                (Role::MessageSelected, Role::Text),
                (Role::Reaction, Role::Text),
                (Role::ReactionOwn, Role::Text),
                (Role::ReactionOwn, Role::Reaction),
                (Role::Spoiler, Role::Text),
                (Role::MessageSelected, Role::RoomCursor),
                (Role::MessageSelected, Role::Reaction),
                (Role::MessageSelected, Role::Spoiler),
            ] {
                if orig_bg(a) != orig_bg(b) {
                    assert_ne!(
                        bg(a),
                        bg(b),
                        "{name}: {a:?} ({:?}) collapsed onto {b:?} ({:?})",
                        bg(a),
                        bg(b)
                    );
                }
            }
        }
    }

    #[test]
    fn closely_spaced_shades_stay_apart_and_ordered_in_256() {
        for (label, bg_hex, surf_hex, hi_hex, fg_hex, lighter_steps) in [
            ("dark", "#010101", "#040404", "#070707", "#ffffff", true),
            ("light", "#ffffff", "#fcfcfc", "#f9f9f9", "#000000", false),
        ] {
            let src = format!(
                "[palette]\nbg = \"{bg_hex}\"\nsurface = \"{surf_hex}\"\nsurface_high = \"{hi_hex}\"\nfg = \"{fg_hex}\"\n"
            );
            let mut theme = Theme::parse(&src, label).unwrap();
            theme.downgrade_to_256();
            let idx = |role: Role| {
                let Some(Color::Indexed(i)) = theme.style(role).bg else {
                    panic!("{label} {role:?}: {:?}", theme.style(role).bg);
                };
                i
            };
            let (t, c, a, m) = (
                idx(Role::Text),
                idx(Role::RoomCursor),
                idx(Role::RoomActive),
                idx(Role::MessageSelected),
            );
            assert_ne!(t, c, "{label}");
            assert_ne!(t, a, "{label}");
            assert_ne!(c, a, "{label}");
            assert_ne!(t, m, "{label}");
            assert_ne!(c, m, "{label}");
            let (lt, lc, la) = (
                luma(palette_256_rgb(t)),
                luma(palette_256_rgb(c)),
                luma(palette_256_rgb(a)),
            );
            if lighter_steps {
                assert!(lt < lc && lc < la, "{label}: {t}({lt}) {c}({lc}) {a}({la})");
            } else {
                assert!(lt > lc && lc > la, "{label}: {t}({lt}) {c}({lc}) {a}({la})");
            }
        }

        // Intentionally identical backgrounds are left equal.
        let mut same = Theme::parse(
            "[palette]\nbg = \"#101010\"\nsurface = \"#101010\"\nfg = \"#ffffff\"\n",
            "same",
        )
        .unwrap();
        same.downgrade_to_256();
        assert_eq!(same.style(Role::Text).bg, same.style(Role::RoomCursor).bg);
    }
}
