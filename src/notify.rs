//! Showing notifications: through the desktop's notification service, or through the
//! terminal where that is the better (or only) way.
//!
//! What a notification says, its icon and where a click on it goes are here. Sending it
//! (the desktop's service over D-Bus, or the terminal's escape sequences) is in
//! [`send`], and the owner-only folder of icon files the desktop is pointed at in
//! [`icon_cache`].

mod icon_cache;
mod send;

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use ratatui::style::Color;
use serde::Deserialize;

use crate::images::Pixels;
use crate::matrix::Notice;
use crate::text::truncate;
use crate::theme::{Role, Theme};

pub use icon_cache::IconCache;
pub use send::{session_bus, set_app_icon, show, show_reminder};

pub const TILE_SIZE: u32 = 96;
const GLYPH_COLS: u32 = 5;
const GLYPH_ROWS: u32 = 7;
const GLYPH_SCALE: u32 = 10;
const GLYPH_ORIGIN_X: u32 = (TILE_SIZE - GLYPH_COLS * GLYPH_SCALE) / 2;
const GLYPH_ORIGIN_Y: u32 = (TILE_SIZE - GLYPH_ROWS * GLYPH_SCALE) / 2;

/// Which icon desktop notifications name as the program's own. (A room's picture, where
/// there is one, is shown as well; that is not this.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppIcon {
    /// matrix-rat's icon in colour.
    #[default]
    Color,
    /// matrix-rat's icon as a line drawing in one colour.
    Outline,
    /// The desktop's own "new message" icon.
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMode {
    /// The desktop's service when there is one, else the terminal.
    #[default]
    Auto,
    Desktop,
    /// Escape sequences the terminal turns into a notification (kitty, iTerm2, foot,
    /// WezTerm…). Also works over SSH.
    Terminal,
    Off,
}

impl NotifyMode {
    pub fn name(self) -> &'static str {
        match self {
            NotifyMode::Auto => "auto",
            NotifyMode::Desktop => "desktop",
            NotifyMode::Terminal => "terminal",
            NotifyMode::Off => "off",
        }
    }
}

/// Title and text as shown: "sender in room" for rooms, the person for direct chats.
pub fn title_and_text(notice: &Notice, preview: bool) -> (String, String) {
    // Without preview nothing of the conversation is shown: not the text, not who
    // wrote, not where.
    if !preview {
        let text = if notice.highlight {
            "You were mentioned"
        } else {
            "New message"
        };
        return ("matrix-rat".to_owned(), text.to_owned());
    }
    let title = if notice.sender.is_empty() || notice.sender == notice.room {
        notice.room.clone()
    } else {
        format!("{} in {}", notice.sender, notice.room)
    };
    (truncate(&title, 120), notice.body.clone())
}

/// Title and text of a reminder's notification (see `crate::saved`). The notice
/// carries the message's sender and text only where they may be shown.
pub fn reminder_title_and_text(due: &DueReminder, preview: bool) -> (String, String) {
    let notice = &due.notice;
    // One for several names none of them, with or without preview.
    if due.count > 1 {
        return (
            "matrix-rat".to_owned(),
            format!("{} reminders are due", due.count),
        );
    }
    // Without preview: not the text, not who wrote, not where.
    if !preview {
        return ("matrix-rat".to_owned(), "A reminder is due".to_owned());
    }
    let title = if notice.room.is_empty() {
        "Reminder".to_owned()
    } else {
        format!("Reminder: {}", notice.room)
    };
    let text = match (notice.sender.is_empty(), notice.body.is_empty()) {
        (_, true) => "A saved message is due".to_owned(),
        (true, false) => notice.body.clone(),
        (false, false) => format!("{}: {}", notice.sender, notice.body),
    };
    (truncate(&title, 120), text)
}

/// Visual icon attached to a desktop notification.
#[derive(Clone, Default)]
pub enum NotifyIcon {
    /// No room image; let the notification server show the generic app icon.
    #[default]
    Generic,
    /// Built-in colored square tile with the room's initial glyph (`A`–`Z`, `0`–`9`, `?`).
    Tile { ch: char, bg: [u8; 3], fg: [u8; 3] },
    /// Pre-decoded avatar image ready to be written as a temporary PNG.
    Picture { key: String, pixels: Pixels },
}

/// First ASCII alphanumeric character of a room name (matching the sidebar's
/// leading ASCII punctuation trim), falling back to `'?'`.
pub fn tile_char(room_name: &str) -> char {
    let trimmed = room_name.trim_start_matches(|c: char| !c.is_alphanumeric() && c.is_ascii());
    trimmed
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii_alphanumeric())
        .unwrap_or('?')
}

/// Picks the room tile's background and readable foreground `[r, g, b]` colours
/// from the current theme (`user_color` + `Role::UnreadBadge.fg`).
pub fn tile_colors(theme: &Theme, room_name: &str) -> ([u8; 3], [u8; 3]) {
    let bg = color_to_rgb(theme.user_color(room_name)).unwrap_or([88, 120, 200]);
    let badge_fg = theme.style(Role::UnreadBadge).fg.and_then(color_to_rgb);
    let fg = match badge_fg {
        Some(candidate) if contrast_ratio(bg, candidate) >= 3.0 => candidate,
        _ => {
            let dark = [22, 22, 29];
            let light = [250, 250, 252];
            if contrast_ratio(bg, dark) >= contrast_ratio(bg, light) {
                dark
            } else {
                light
            }
        }
    };
    (bg, fg)
}

/// Builds a [`NotifyIcon::Tile`] for `room_name` under `theme`.
pub fn tile_icon(theme: &Theme, room_name: &str) -> NotifyIcon {
    let (bg, fg) = tile_colors(theme, room_name);
    NotifyIcon::Tile {
        ch: tile_char(room_name),
        bg,
        fg,
    }
}

/// Renders a crisp `96x96` colored letter tile using a built-in `5x7` bitmap font.
pub fn render_tile_image(ch: char, bg: [u8; 3], fg: [u8; 3]) -> DynamicImage {
    let rows = glyph_rows(ch);
    let bg_px = Rgb(bg);
    let fg_px = Rgb(fg);
    let img = RgbImage::from_fn(TILE_SIZE, TILE_SIZE, |x, y| {
        if let (Some(gx), Some(gy)) = (x.checked_sub(GLYPH_ORIGIN_X), y.checked_sub(GLYPH_ORIGIN_Y))
        {
            let col = gx / GLYPH_SCALE;
            let row = gy / GLYPH_SCALE;
            if col < GLYPH_COLS && row < GLYPH_ROWS {
                let bit = 1u8 << (GLYPH_COLS - 1 - col);
                if (rows[row as usize] & bit) != 0 {
                    return fg_px;
                }
            }
        }
        bg_px
    });
    DynamicImage::ImageRgb8(img)
}

/// What a click on a notification means: each notification carries its own, so that
/// a click goes where that notification pointed, whatever was shown after it.
#[derive(Clone, PartialEq, Eq)]
pub struct Click {
    /// The room to open. Empty for the notification of several reminders, which
    /// opens the list of saved messages.
    pub room_id: String,
    /// For a reminder: the message to go to, and the thread it is in.
    pub event: Option<(String, Option<String>)>,
}

/// Hand-written: no room and no message in logs.
impl std::fmt::Debug for Click {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Click(to a message: {})", self.event.is_some())
    }
}

impl From<&str> for Click {
    fn from(room_id: &str) -> Self {
        Self {
            room_id: room_id.to_owned(),
            event: None,
        }
    }
}

/// A reminder's notification on its way out (see `crate::saved`).
pub struct DueReminder {
    /// Room, and sender and text only where they may be shown.
    pub notice: Notice,
    /// The message it is for, and its thread; `None` for the one that stands for
    /// several.
    pub event: Option<(String, Option<String>)>,
    /// How many reminders it stands for.
    pub count: usize,
}

/// Hand-written: only the count.
impl std::fmt::Debug for DueReminder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DueReminder({})", self.count)
    }
}

/// Where the clicks on notifications go: to the event loop.
pub type Clicks = tokio::sync::mpsc::UnboundedSender<Click>;

fn icon_cache_key(icon: &NotifyIcon) -> Option<Vec<u8>> {
    match icon {
        NotifyIcon::Generic => None,
        NotifyIcon::Tile { ch, bg, fg } => {
            let mut key = Vec::with_capacity(12);
            key.extend_from_slice(b"tile\0");
            key.push(*ch as u8);
            key.extend_from_slice(bg);
            key.extend_from_slice(fg);
            Some(key)
        }
        NotifyIcon::Picture { key, .. } => {
            let mut out = Vec::with_capacity(7 + key.len());
            out.extend_from_slice(b"avatar\0");
            out.extend_from_slice(key.as_bytes());
            Some(out)
        }
    }
}

fn encode_icon_png(icon: &NotifyIcon) -> Result<Vec<u8>, image::ImageError> {
    let mut bytes = Vec::new();
    match icon {
        NotifyIcon::Generic => {}
        NotifyIcon::Tile { ch, bg, fg } => {
            render_tile_image(*ch, *bg, *fg)
                .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)?;
        }
        NotifyIcon::Picture { pixels, .. } => {
            pixels.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)?;
        }
    }
    Ok(bytes)
}

fn cache_filename(key: &[u8]) -> String {
    let mut hash: u128 = 0x6c62272e07bb014262b821756295c58d;
    for &b in key {
        hash ^= b as u128;
        hash = hash.wrapping_mul(0x0000000001000000000000000000013b);
    }
    format!("{hash:032x}.png")
}

fn color_to_rgb(color: Color) -> Option<[u8; 3]> {
    match color {
        Color::Reset => None,
        Color::Black => Some([24, 24, 28]),
        Color::Red => Some([205, 49, 49]),
        Color::Green => Some([13, 188, 121]),
        Color::Yellow => Some([229, 229, 16]),
        Color::Blue => Some([36, 114, 200]),
        Color::Magenta => Some([188, 63, 188]),
        Color::Cyan => Some([17, 168, 205]),
        Color::Gray => Some([204, 204, 204]),
        Color::DarkGray => Some([102, 102, 102]),
        Color::LightRed => Some([241, 76, 76]),
        Color::LightGreen => Some([35, 209, 139]),
        Color::LightYellow => Some([245, 245, 67]),
        Color::LightBlue => Some([59, 142, 234]),
        Color::LightMagenta => Some([214, 112, 214]),
        Color::LightCyan => Some([41, 184, 219]),
        Color::White => Some([250, 250, 250]),
        Color::Rgb(r, g, b) => Some([r, g, b]),
        Color::Indexed(i) => Some(indexed_to_rgb(i)),
    }
}

fn indexed_to_rgb(idx: u8) -> [u8; 3] {
    const ANSI16: [[u8; 3]; 16] = [
        [24, 24, 28],
        [205, 49, 49],
        [13, 188, 121],
        [229, 229, 16],
        [36, 114, 200],
        [188, 63, 188],
        [17, 168, 205],
        [204, 204, 204],
        [102, 102, 102],
        [241, 76, 76],
        [35, 209, 139],
        [245, 245, 67],
        [59, 142, 234],
        [214, 112, 214],
        [41, 184, 219],
        [250, 250, 250],
    ];
    match idx {
        0..=15 => ANSI16[idx as usize],
        16..=231 => {
            let n = idx - 16;
            let step = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            [step(n / 36), step((n % 36) / 6), step(n % 6)]
        }
        232..=255 => {
            let v = 8 + 10 * (idx - 232);
            [v, v, v]
        }
    }
}

fn relative_luminance(rgb: [u8; 3]) -> f32 {
    let channel = |c: u8| {
        let s = f32::from(c) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn glyph_rows(ch: char) -> [u8; 7] {
    match ch {
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        _ => [
            0b01110, 0b10001, 0b00001, 0b00110, 0b00100, 0b00000, 0b00100,
        ],
    }
}

/// Notification servers read the text as simple markup.
fn escape_markup(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// OSC 99 (kitty's protocol, with title and text) or the older OSC 9 (one line).
/// The text is from other people: nothing that could end the sequence may be in it.
fn terminal_sequence(title: &str, text: &str, kitty: bool, id: u32) -> String {
    let clean = |s: &str| -> String {
        crate::text::sanitize_line(s)
            .chars()
            .filter(|c| !c.is_control())
            .collect()
    };
    let (title, text) = (clean(title), clean(text));
    if kitty {
        // Each notification has its own id, so a new one does not replace an unread one.
        format!(
            "\x1b]99;i=mr{id}:d=0:p=title;{title}\x1b\\\x1b]99;i=mr{id}:d=1:p=body;{text}\x1b\\"
        )
    } else {
        // In OSC 9 a leading "4;" would be read as a progress report, so no text of
        // ours may put a semicolon where the terminal looks for one.
        let line = format!("{title}: {text}").replace(';', ",");
        format!("\x1b]9;{line}\x1b\\")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn the_icon_setting_reads_its_three_values() {
        for (text, icon) in [
            ("color", AppIcon::Color),
            ("outline", AppIcon::Outline),
            ("generic", AppIcon::Generic),
        ] {
            let config: crate::config::Config =
                toml::from_str(&format!("notification_icon = \"{text}\"")).unwrap();
            assert_eq!(config.notification_icon, icon);
        }
        assert_eq!(
            crate::config::Config::default().notification_icon,
            AppIcon::Color
        );
        assert!(toml::from_str::<crate::config::Config>("notification_icon = \"big\"").is_err());
    }

    pub(super) fn notice(room: &str, sender: &str, body: &str, highlight: bool) -> Notice {
        Notice {
            room_id: "!a".into(),
            room: room.into(),
            sender: sender.into(),
            body: body.into(),
            highlight,
            thread: None,
        }
    }

    #[test]
    fn a_reminder_shows_its_message_only_with_previews_on() {
        let due = |notice: Notice, count: usize| DueReminder {
            notice,
            event: Some(("$e".into(), None)),
            count,
        };
        let reminder = due(notice("Rust", "ann", "the secret plan", false), 1);
        assert_eq!(
            reminder_title_and_text(&reminder, true),
            (
                "Reminder: Rust".to_owned(),
                "ann: the secret plan".to_owned()
            )
        );
        // Without preview: no text, no sender, no room.
        assert_eq!(
            reminder_title_and_text(&reminder, false),
            ("matrix-rat".to_owned(), "A reminder is due".to_owned())
        );
        // A message whose text is not known (deleted, not loaded, a room that was left).
        let bare = due(notice("", "", "", false), 1);
        assert_eq!(
            reminder_title_and_text(&bare, true),
            ("Reminder".to_owned(), "A saved message is due".to_owned())
        );
        let nameless = due(notice("Rust", "", "text", false), 1);
        assert_eq!(reminder_title_and_text(&nameless, true).1, "text");
        // One notification for several: a count, whatever the notice holds.
        let many = due(notice("Rust", "ann", "the secret plan", false), 5);
        for preview in [true, false] {
            assert_eq!(
                reminder_title_and_text(&many, preview),
                ("matrix-rat".to_owned(), "5 reminders are due".to_owned())
            );
        }
        let shown = format!("{many:?} {:?}", Click::from("!secret:example.org"));
        assert!(
            !shown.contains("secret") && !shown.contains("ann"),
            "{shown}"
        );
    }

    #[test]
    fn titles_and_previews() {
        let room = notice("Rust", "ann", "hello", false);
        assert_eq!(
            title_and_text(&room, true),
            ("ann in Rust".to_owned(), "hello".to_owned())
        );
        // A direct chat is named after the person; no "ann in ann".
        let direct = notice("ann", "ann", "hello", false);
        assert_eq!(title_and_text(&direct, true).0, "ann");
        // Without preview nothing of the conversation shows: no text, sender or room.
        assert_eq!(
            title_and_text(&room, false),
            ("matrix-rat".to_owned(), "New message".to_owned())
        );
        let mention = notice("Rust", "ann", "you: look", true);
        assert_eq!(title_and_text(&mention, false).1, "You were mentioned");
    }

    #[test]
    fn nothing_from_a_message_can_break_out() {
        let sequence = terminal_sequence("a\x1b]52;c;evil\x07", "b\x1b\\c\nd", true, 7);
        // Exactly the two sequences we wrote: two starts, two ends.
        assert_eq!(sequence.matches('\x1b').count(), 4);
        assert!(!sequence.contains('\x07') && !sequence.contains('\n'));
        assert!(sequence.contains("i=mr7:d=0:p=title;") && sequence.contains("i=mr7:d=1:p=body;"));
        assert_eq!(terminal_sequence("t", "x", false, 1), "\x1b]9;t: x\x1b\\");
        // A name must not turn the notification into a progress report (OSC 9;4).
        assert_eq!(
            terminal_sequence("4;3;50", "x;y", false, 1),
            "\x1b]9;4,3,50: x,y\x1b\\"
        );
        assert_eq!(
            escape_markup("<b>a & b</b>"),
            "&lt;b&gt;a &amp; b&lt;/b&gt;"
        );
    }

    /// A fresh empty folder for one test, removed at its end.
    pub(super) struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(1);
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir =
                std::env::temp_dir().join(format!("matrix-rat-{name}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn glyph_pattern_from_png(ch: char) -> String {
        let bg = [20, 30, 40];
        let fg = [245, 245, 245];
        let img = render_tile_image(ch, bg, fg);
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (TILE_SIZE, TILE_SIZE));
        assert_eq!(decoded.get_pixel(0, 0).0, bg);
        assert_eq!(decoded.get_pixel(TILE_SIZE - 1, TILE_SIZE - 1).0, bg);
        let mut out = String::new();
        for row in 0..GLYPH_ROWS {
            for col in 0..GLYPH_COLS {
                let x = GLYPH_ORIGIN_X + col * GLYPH_SCALE + GLYPH_SCALE / 2;
                let y = GLYPH_ORIGIN_Y + row * GLYPH_SCALE + GLYPH_SCALE / 2;
                let px = decoded.get_pixel(x, y).0;
                assert!(px == fg || px == bg, "unexpected anti-aliased pixel {px:?}");
                out.push(if px == fg { '#' } else { '.' });
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn tile_chars_and_bitmap_patterns() {
        assert_eq!(tile_char("#rust:matrix.org"), 'R');
        assert_eq!(tile_char("42 Lounge"), '4');
        assert_eq!(tile_char("  @ann"), 'A');
        assert_eq!(tile_char("Москва"), '?');
        assert_eq!(tile_char("🚀 Launch"), '?');
        assert_eq!(tile_char("---"), '?');
        assert_eq!(tile_char(""), '?');

        let rendered = format!(
            "R:\n{}\n4:\n{}\n?:\n{}",
            glyph_pattern_from_png(tile_char("#rust:matrix.org")),
            glyph_pattern_from_png(tile_char("42 Lounge")),
            glyph_pattern_from_png(tile_char("Москва")),
        );
        insta::assert_snapshot!("tile_bitmap_patterns", rendered);
    }

    #[test]
    fn tile_colors_maintain_readability_across_builtin_themes() {
        for name in Theme::builtin_names() {
            let theme = Theme::builtin(&name).unwrap();
            for room in ["#rust", "Alice", "General", "Deployments", "Café"] {
                let (bg, fg) = tile_colors(&theme, room);
                assert!(
                    contrast_ratio(bg, fg) >= 3.0,
                    "theme {name} room {room}: low contrast between bg={bg:?} and fg={fg:?}"
                );
            }
        }
    }
}
