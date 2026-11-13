//! Showing notifications: through the desktop's notification service, or through the
//! terminal where that is the better (or only) way.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{Cursor, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use ratatui::style::Color;
use serde::Deserialize;

use crate::images::Pixels;
use crate::matrix::Notice;
use crate::text::truncate;
use crate::theme::{Role, Theme};

pub const TILE_SIZE: u32 = 96;
const GLYPH_COLS: u32 = 5;
const GLYPH_ROWS: u32 = 7;
const GLYPH_SCALE: u32 = 10;
const GLYPH_ORIGIN_X: u32 = (TILE_SIZE - GLYPH_COLS * GLYPH_SCALE) / 2;
const GLYPH_ORIGIN_Y: u32 = (TILE_SIZE - GLYPH_ROWS * GLYPH_SCALE) / 2;
const DEFAULT_CACHE_LIMIT: usize = 64;

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

    /// What `Auto` means on this machine.
    pub fn resolved(self) -> NotifyMode {
        if self != NotifyMode::Auto {
            return self;
        }
        let remote = crate::portal::in_ssh_session();
        let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some();
        if bus && !remote {
            NotifyMode::Desktop
        } else {
            NotifyMode::Terminal
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

/// Thread-safe, bounded, symlink-safe cache of `0o600` PNG notification icons in `runtime_dir`.
pub struct IconCache {
    state: Mutex<CacheState>,
}

impl std::fmt::Debug for IconCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IconCache").finish_non_exhaustive()
    }
}

struct CacheState {
    dir: PathBuf,
    limit: usize,
    epoch: u64,
    next_tmp: u64,
    /// Ordered from least recently used (`front`) to most recently used (`back`).
    entries: VecDeque<(Vec<u8>, PathBuf)>,
}

impl IconCache {
    /// Creates an icon cache rooted at `dir`, clearing any leftover files from a prior run.
    pub fn new(dir: PathBuf) -> Self {
        Self::with_limit(dir, DEFAULT_CACHE_LIMIT)
    }

    pub fn with_limit(dir: PathBuf, limit: usize) -> Self {
        let cache = Self {
            state: Mutex::new(CacheState {
                dir,
                limit: limit.max(1),
                epoch: 0,
                next_tmp: 0,
                entries: VecDeque::new(),
            }),
        };
        cache.clear();
        cache
    }

    /// Monotonic session epoch, incremented on [`Self::clear`] so stale queued jobs do not
    /// recreate cache files after logout or shutdown.
    pub fn epoch(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).epoch
    }

    /// Bumps the cache epoch and removes all cached PNGs and the cache directory (without
    /// following symlinks).
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = state.epoch.wrapping_add(1);
        state.entries.clear();
        let Ok(meta) = std::fs::symlink_metadata(&state.dir) else {
            return;
        };
        if !meta.file_type().is_dir() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&state.dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let _ = std::fs::remove_dir(&state.dir);
    }

    /// Ensures a PNG file exists for `icon` when `expected_epoch` still matches the active
    /// session, enforcing `0o700` dir and `0o600` files without following symlinks. Returns
    /// `None` for [`NotifyIcon::Generic`], stale epochs, or any I/O failure.
    pub fn ensure_file(&self, icon: &NotifyIcon, expected_epoch: u64) -> Option<PathBuf> {
        let key = icon_cache_key(icon)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.epoch != expected_epoch {
            return None;
        }
        if let Some(pos) = state.entries.iter().position(|(k, _)| k == &key) {
            let path = state.entries[pos].1.clone();
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
                let entry = state.entries.remove(pos)?;
                state.entries.push_back(entry);
                return Some(path);
            }
            state.entries.remove(pos);
        }
        if let Err(err) = ensure_safe_dir(&state.dir) {
            tracing::debug!(%err, dir = %state.dir.display(), "notification icon directory unavailable");
            return None;
        }
        let filename = cache_filename(&key);
        let path = state.dir.join(&filename);
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file()) {
            let png = match encode_icon_png(icon) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::debug!(%err, "could not encode notification icon");
                    return None;
                }
            };
            state.next_tmp = state.next_tmp.wrapping_add(1);
            let tmp = state.dir.join(format!(
                ".{filename}.{}.{}.tmp",
                std::process::id(),
                state.next_tmp
            ));
            if let Err(err) =
                write_private_file(&tmp, &png).and_then(|()| std::fs::rename(&tmp, &path))
            {
                let _ = std::fs::remove_file(&tmp);
                tracing::debug!(%err, "could not write notification icon file");
                return None;
            }
        }
        state.entries.push_back((key, path.clone()));
        while state.entries.len() > state.limit {
            if let Some((_, old_path)) = state.entries.pop_front() {
                let _ = std::fs::remove_file(old_path);
            }
        }
        Some(path)
    }
}

struct DesktopJob {
    title: String,
    text: String,
    urgent: bool,
    preview: bool,
    icon: NotifyIcon,
    cache: Option<(Arc<IconCache>, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreparedDesktop {
    summary: String,
    body: String,
    urgent: bool,
    icon_path: Option<PathBuf>,
}

fn prepare_desktop(job: &DesktopJob) -> PreparedDesktop {
    let icon_path = if job.preview {
        job.cache
            .as_ref()
            .and_then(|(cache, epoch)| cache.ensure_file(&job.icon, *epoch))
    } else {
        None
    };
    PreparedDesktop {
        summary: escape_markup(&job.title),
        body: escape_markup(&job.text),
        urgent: job.urgent,
        icon_path,
    }
}

/// Shows a notification. Never blocks and never fails loudly: a notification that
/// cannot be shown is only logged. For desktop notifications, attaches an optional room
/// picture or generated letter tile (`preview = false` and terminal modes ignore `icon`
/// and never touch `cache`).
pub fn show(
    mode: NotifyMode,
    notice: &Notice,
    preview: bool,
    icon: &NotifyIcon,
    cache: Option<(Arc<IconCache>, u64)>,
) {
    let (title, text) = title_and_text(notice, preview);
    match mode.resolved() {
        NotifyMode::Desktop => {
            let _ = desktop().send(DesktopJob {
                title,
                text,
                urgent: notice.highlight,
                preview,
                icon: if preview {
                    icon.clone()
                } else {
                    NotifyIcon::Generic
                },
                cache: if preview { cache } else { None },
            });
        }
        NotifyMode::Terminal => {
            let mut out = std::io::stdout();
            let id = SERIAL.fetch_add(1, Ordering::Relaxed);
            let _ = out
                .write_all(terminal_sequence(&title, &text, kitty(), id).as_bytes())
                .and_then(|()| out.flush());
        }
        NotifyMode::Auto | NotifyMode::Off => {}
    }
}

static SERIAL: AtomicU32 = AtomicU32::new(1);

/// One thread shows all desktop notifications, one after the other: the service may be
/// slow or absent, which must not hold up the interface, and a burst must not start a
/// thread and a bus connection each.
fn desktop() -> &'static std::sync::mpsc::Sender<DesktopJob> {
    static SENDER: std::sync::OnceLock<std::sync::mpsc::Sender<DesktopJob>> =
        std::sync::OnceLock::new();
    SENDER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<DesktopJob>();
        std::thread::spawn(move || {
            for job in receiver {
                let prepared = prepare_desktop(&job);
                let mut notification = notify_rust::Notification::new();
                notification
                    .appname("matrix-rat")
                    .summary(&prepared.summary)
                    .body(&prepared.body)
                    .icon("mail-message-new");
                if let Some(path) = prepared.icon_path.as_deref().and_then(|p| p.to_str()) {
                    notification.image_path(path);
                }
                if prepared.urgent {
                    notification.urgency(notify_rust::Urgency::Critical);
                }
                if let Err(err) = notification.show() {
                    tracing::debug!(%err, "could not show a desktop notification");
                }
            }
        });
        sender
    })
}

fn ensure_safe_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.file_type().is_dir() {
                return Err(std::io::Error::other("icon cache path is not a directory"));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            let meta = std::fs::symlink_metadata(dir)?;
            if !meta.file_type().is_dir() {
                return Err(std::io::Error::other("icon cache path is not a directory"));
            }
        }
        Err(err) => return Err(err),
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

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

fn kitty() -> bool {
    std::env::var_os("KITTY_WINDOW_ID").is_some()
        || std::env::var("TERM").is_ok_and(|term| term.contains("kitty"))
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

    fn notice(room: &str, sender: &str, body: &str, highlight: bool) -> Notice {
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

    #[test]
    fn explicit_modes_are_kept() {
        assert_eq!(NotifyMode::Off.resolved(), NotifyMode::Off);
        assert_eq!(NotifyMode::Terminal.resolved(), NotifyMode::Terminal);
        assert_ne!(NotifyMode::Auto.resolved(), NotifyMode::Auto);
        let config: crate::config::Config = toml::from_str("notifications = \"terminal\"").unwrap();
        assert_eq!(config.notifications, NotifyMode::Terminal);
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
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

    #[test]
    fn icon_cache_permissions_lru_eviction_and_cleanup() {
        let root = TempDir::new("icon-cache");
        let dir = root.0.join("cache");
        let cache = IconCache::with_limit(dir.clone(), 2);
        let epoch = cache.epoch();

        let tile_a = NotifyIcon::Tile {
            ch: 'A',
            bg: [10, 20, 30],
            fg: [240, 240, 240],
        };
        let tile_b = NotifyIcon::Tile {
            ch: 'B',
            bg: [20, 30, 40],
            fg: [240, 240, 240],
        };
        let tile_c = NotifyIcon::Tile {
            ch: 'C',
            bg: [30, 40, 50],
            fg: [240, 240, 240],
        };

        let path_a = cache.ensure_file(&tile_a, epoch).unwrap();
        let path_b = cache.ensure_file(&tile_b, epoch).unwrap();
        assert_ne!(path_a, path_b);
        assert_eq!(
            std::fs::symlink_metadata(&dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&path_a)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(image::open(&path_a).unwrap().width(), TILE_SIZE);

        // Touch A so B becomes least recently used, then insert C: B must be evicted.
        assert_eq!(cache.ensure_file(&tile_a, epoch), Some(path_a.clone()));
        let path_c = cache.ensure_file(&tile_c, epoch).unwrap();
        assert!(path_a.exists());
        assert!(!path_b.exists());
        assert!(path_c.exists());

        // Clear wipes files and dir and bumps epoch so stale queued jobs do not rewrite.
        cache.clear();
        assert!(!dir.exists());
        assert_ne!(cache.epoch(), epoch);
        assert_eq!(cache.ensure_file(&tile_a, epoch), None);
        assert!(!dir.exists());
    }

    #[test]
    fn preview_off_terminal_and_cache_failures_do_not_break_notifications() {
        let root = TempDir::new("icon-fallback");
        let dir = root.0.join("cache");
        let cache = Arc::new(IconCache::new(dir.clone()));
        let epoch = cache.epoch();
        let tile = NotifyIcon::Tile {
            ch: 'R',
            bg: [40, 60, 120],
            fg: [250, 250, 250],
        };

        // Preview disabled hides the picture and never creates the cache dir.
        let hidden = prepare_desktop(&DesktopJob {
            title: "matrix-rat".into(),
            text: "New message".into(),
            urgent: false,
            preview: false,
            icon: tile.clone(),
            cache: Some((Arc::clone(&cache), epoch)),
        });
        assert_eq!(hidden.icon_path, None);
        assert!(!dir.exists());

        // Terminal mode leaves the icon cache untouched.
        show(
            NotifyMode::Terminal,
            &notice("Rust", "ann", "hi", false),
            true,
            &tile,
            Some((Arc::clone(&cache), epoch)),
        );
        assert!(!dir.exists());

        // A blocked cache path (symlink instead of directory) still yields a sendable notice.
        let target = root.0.join("outside");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &dir).unwrap();
        let prepared = prepare_desktop(&DesktopJob {
            title: "ann in Rust".into(),
            text: "hello".into(),
            urgent: true,
            preview: true,
            icon: tile,
            cache: Some((cache, epoch)),
        });
        assert_eq!(prepared.summary, "ann in Rust");
        assert_eq!(prepared.body, "hello");
        assert!(prepared.urgent);
        assert_eq!(prepared.icon_path, None);
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }
}
