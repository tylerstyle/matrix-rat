//! Pictures: what the terminal can draw, how large a picture is on screen, decoding,
//! and the cache of decoded and encoded pictures. The UI thread owns the cache; the
//! Matrix worker only fetches and decodes.
//!
//! With the kitty protocol a picture is sent to the terminal once and then shown through
//! ordinary cells (Unicode placeholders), so scrolling, cropping and popups on top work
//! like for text. Everywhere else pictures are drawn with coloured half blocks.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, Rgba, RgbaImage};
use ratatui::layout::Size;
use ratatui_image::protocol::halfblocks::Halfblocks;
use ratatui_image::protocol::kitty::Kitty;
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FontSize, Resize};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::ImageMode;
use crate::model::{DEMO_IMAGE, ImageInfo, ImageSource, MediaRef};
use crate::termquery;

/// The largest picture drawn between messages, in cells.
pub const MAX_COLS: u16 = 40;
pub const MAX_ROWS: u16 = 12;
/// A sticker is a small picture: it gets less room than a photo.
const STICKER_MAX_CELLS: (u16, u16) = (18, 7);
/// Space kept for a picture whose size is not known yet.
const UNKNOWN_CELLS: (u16, u16) = (24, 8);
/// Pictures between messages are kept no larger than this, in pixels.
pub const THUMBNAIL_PIXELS: (u32, u32) = (640, 480);
/// The viewer never needs more than a large screen's worth.
pub const FULL_PIXELS: (u32, u32) = (3840, 2160);
/// Files above this are not downloaded.
pub const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
/// Decoding refuses pictures wider or taller than this (decompression bombs).
pub const MAX_DECODE_SIDE: u32 = 16_384;
pub const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
/// Memory the cache may hold before the pictures unused for longest are dropped.
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const CACHE_ENTRIES: usize = 256;
/// Room pictures are kept as squares of this size; they are shown in two cells (four
/// by two in the room list with room previews), which is some 20 to 80 pixels each way.
pub const AVATAR_PIXELS: (u32, u32) = (96, 96);
/// Room pictures kept; at 96x96 each that is some 20 MB at the very most. On top of
/// that each keeps, for each size it is drawn at, what the terminal is sent: kitty's
/// base64 of the pixels of those cells, which is a third more than their four bytes
/// each. That is at most [`AVATAR_SIZES`] for each, so 512 x 3 in all; with cells of
/// 10 by 20 pixels, some 2 KB for two cells and 9 KB for four by two, some 6 MB for
/// all 512 at both. It grows with the pixels of a cell.
const AVATAR_ENTRIES: usize = 512;
/// Sizes a room picture is kept made for at once: it is drawn at two (see
/// [`ImageStore::avatar`]), and one more is spare. A frame that draws it at more
/// sizes keeps them all until a later frame draws fewer.
const AVATAR_SIZES: usize = 3;
/// Custom emoji are fetched as thumbnails no larger than this.
pub const EMOTE_PIXELS: (u32, u32) = (64, 64);
/// Columns and rows a custom emoji's picture takes in a line of text or on a reaction:
/// the width of an ordinary emoji, so lines keep their layout.
pub const EMOTE_CELLS: (u16, u16) = (2, 1);
/// Custom emoji pictures kept; at 64x64 each that is some 4 MB at the very most.
const EMOTE_ENTRIES: usize = 256;
/// Custom emoji pictures on their way at once. A screen full of new ones is fetched a
/// few at a time, the rest in later frames.
pub const EMOTE_IN_FLIGHT: usize = 6;
/// Sizes remembered for pictures whose message did not state one.
const LEARNED_ENTRIES: usize = 4096;
/// A request the worker never answered (it restarted) may be made again after this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the terminal gets to answer the question about what it can draw.
const QUERY_TIMEOUT: Duration = Duration::from_millis(1000);
/// Above this many pixels a kitty transmission is compressed, if the terminal can.
const COMPRESS_ABOVE: u32 = 512 * 512;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub type Pixels = Arc<DynamicImage>;

/// A decoded picture, scaled down to what is useful on a screen.
#[derive(Clone)]
pub struct Decoded {
    pub pixels: Pixels,
    /// Pixel size of the file before scaling down.
    pub original: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Kitty {
        /// The terminal accepts compressed transmissions.
        compress: bool,
    },
    Halfblocks,
}

/// How pictures get onto this terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Graphics {
    /// `None`: pictures are off.
    pub protocol: Option<Protocol>,
    /// Size of one cell in pixels. A guess with the right proportions for half blocks.
    pub font: (u16, u16),
}

const NOMINAL_FONT: (u16, u16) = (10, 20);

impl Graphics {
    pub fn off() -> Self {
        Self {
            protocol: None,
            font: NOMINAL_FONT,
        }
    }

    pub fn halfblocks() -> Self {
        Self {
            protocol: Some(Protocol::Halfblocks),
            font: NOMINAL_FONT,
        }
    }

    /// Asks the terminal what it can do (see [`crate::termquery`] for when this may
    /// run). Takes at most [`QUERY_TIMEOUT`]; without an answer it falls back to
    /// half blocks for graphics. When `probe_appearance` is true, colour-scheme and
    /// background probes are batched into the same round-trip (or run alone when
    /// graphics probing is disabled).
    ///
    /// Only kitty's protocol with Unicode placeholders is used for real graphics: its
    /// pictures live in ordinary cells, so they cannot be left behind or painted over
    /// popups. Terminals that only do sixel or the iTerm2 protocol get half blocks, and
    /// so does everything inside a multiplexer.
    pub fn detect_with_term(
        mode: ImageMode,
        probe_appearance: bool,
    ) -> (Self, Option<termquery::Reply>) {
        let probe_graphics =
            matches!(mode, ImageMode::Auto) && !in_multiplexer() && !placeholders_broken();
        if !probe_graphics {
            let graphics = match mode {
                ImageMode::Off => Self::off(),
                ImageMode::Halfblocks => Self::halfblocks(),
                ImageMode::Auto if in_multiplexer() => {
                    tracing::info!("images: inside a terminal multiplexer, using half blocks");
                    Self::halfblocks()
                }
                ImageMode::Auto => {
                    tracing::info!(
                        "images: this terminal's kitty placeholders are unreliable, using half blocks"
                    );
                    Self::halfblocks()
                }
            };
            let reply = if probe_appearance {
                let reply = termquery::ask_appearance(QUERY_TIMEOUT);
                if let Some(reply) = reply {
                    tracing::info!(?reply, "appearance: terminal queried");
                }
                reply
            } else {
                None
            };
            return (graphics, reply);
        }
        let reply = if probe_appearance {
            termquery::ask(QUERY_TIMEOUT)
        } else {
            termquery::ask_graphics(QUERY_TIMEOUT)
        };
        let Some(reply) = reply else {
            return (Self::halfblocks(), None);
        };
        let graphics = Self::from_reply(reply, termquery::cell_from_window());
        tracing::info!(?reply, ?graphics, "images: terminal queried");
        (graphics, Some(reply))
    }

    /// `window_cell`: the cell size the system reports, for terminals that do not
    /// answer that part.
    fn from_reply(reply: termquery::Reply, window_cell: Option<(u16, u16)>) -> Self {
        match (reply.kitty, reply.cell.or(window_cell)) {
            (true, Some(font)) => Self {
                protocol: Some(Protocol::Kitty {
                    compress: reply.compression,
                }),
                font,
            },
            // No graphics, no answer in time, or no way to size a picture.
            _ => Self::halfblocks(),
        }
    }
}

/// WezTerm and Konsole answer the kitty graphics check but do not place pictures
/// through Unicode placeholders correctly.
pub(crate) fn placeholders_broken() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    set("WEZTERM_EXECUTABLE") || set("KONSOLE_VERSION")
}

pub(crate) fn in_multiplexer() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let term = std::env::var("TERM").unwrap_or_default();
    set("TMUX")
        || set("ZELLIJ")
        || set("STY")
        || term.starts_with("tmux")
        || term.starts_with("screen")
        || std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "tmux")
}

/// The cells a picture of `pixels` takes when it must fit into `max` cells: scaled down
/// to fit, proportions kept, never scaled up.
pub fn fit_cells(pixels: (u32, u32), font: (u16, u16), max: (u16, u16)) -> (u16, u16) {
    let (max_cols, max_rows) = (max.0.max(1), max.1.max(1));
    let (width, height) = (f64::from(pixels.0.max(1)), f64::from(pixels.1.max(1)));
    let (font_width, font_height) = (f64::from(font.0.max(1)), f64::from(font.1.max(1)));
    let scale = (f64::from(max_cols) * font_width / width)
        .min(f64::from(max_rows) * font_height / height)
        .min(1.0);
    // A hair is taken off so that an exact fit does not round up to one more cell.
    let cells = |pixels: f64, cell: f64, max: u16| {
        ((pixels * scale / cell) - 1e-6)
            .ceil()
            .clamp(1.0, f64::from(max)) as u16
    };
    (
        cells(width, font_width, max_cols),
        cells(height, font_height, max_rows),
    )
}

/// The most cells a picture may take between messages in a pane this wide.
pub fn inline_limit(width: usize) -> (u16, u16) {
    let cols = MAX_COLS
        .min(u16::try_from(width).unwrap_or(u16::MAX))
        .max(1);
    // A narrow pane also gets less height, or one picture would fill the screen.
    (cols, MAX_ROWS.min((cols / 3).max(3)))
}

/// Decodes a downloaded file and scales it down to fit `max` pixels. Runs on a blocking
/// thread. The error is a short reason for the user.
pub fn decode(bytes: &[u8], max: (u32, u32)) -> Result<Decoded, String> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "not a picture this app can read".to_owned())?;
    reader.limits(limits);
    let reason = |err: image::ImageError| {
        #[expect(clippy::wildcard_enum_match_arm, reason = "foreign image::ImageError")]
        match err {
            image::ImageError::Limits(_) => "too large to show".to_owned(),
            image::ImageError::Unsupported(_) => "not a picture this app can read".to_owned(),
            _ => "the file is damaged".to_owned(),
        }
    };
    // The decoder is taken apart from the reader to get at the orientation a camera
    // noted; the reader's own check of the memory needed has to be repeated then.
    let mut decoder = reader.into_decoder().map_err(reason)?;
    // Whatever the file's own format, the pixels may end up as four bytes each
    // (turning the picture, scaling it), so that is what has to fit.
    let (width, height) = decoder.dimensions();
    let as_rgba = u64::from(width) * u64::from(height) * 4;
    if decoder.total_bytes().max(as_rgba) > MAX_DECODE_ALLOC {
        return Err("too large to show".to_owned());
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = DynamicImage::from_decoder(decoder).map_err(reason)?;
    image.apply_orientation(orientation);
    let original = (image.width(), image.height());
    if original.0 == 0 || original.1 == 0 {
        return Err("the file is damaged".to_owned());
    }
    let image = if original.0 > max.0 || original.1 > max.1 {
        // `thumbnail` is the fast way down; quality matters little at this size.
        image.thumbnail(max.0, max.1)
    } else {
        image
    };
    // One pixel format from here on.
    let pixels = Arc::new(DynamicImage::ImageRgba8(image.into_rgba8()));
    Ok(Decoded { pixels, original })
}

/// The picture of the demo data: a palette, warm to cool, light at the top.
pub fn demo_picture(width: u32, height: u32) -> DynamicImage {
    let (width, height) = (width.max(1), height.max(1));
    let image = RgbaImage::from_fn(width, height, |x, y| {
        let across = x as f32 / width as f32;
        let down = y as f32 / height as f32;
        // Eight hard-edged swatches, shaded smoothly from top to bottom.
        let swatch = (across * 8.0).floor() / 8.0;
        let hue = 20.0 + swatch * 230.0;
        let lightness = 0.78 - down * 0.5;
        let [r, g, b] = hsl(hue, 0.62, lightness);
        Rgba([r, g, b, 255])
    });
    DynamicImage::ImageRgba8(image)
}

fn hsl(hue: f32, saturation: f32, lightness: f32) -> [u8; 3] {
    let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let sector = (hue.rem_euclid(360.0)) / 60.0;
    let x = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
    let (r, g, b) = match sector as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let lift = lightness - chroma / 2.0;
    [r, g, b].map(|value| ((value + lift).clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// A picture turned into what the terminal draws, for one size in cells.
pub struct Encoded {
    pub size: (u16, u16),
    pub protocol: SlicedProtocol,
    /// The id the terminal knows the picture by (kitty only).
    kitty_id: Option<u32>,
    /// The terminal has received it. Until then the picture travels inside the first
    /// cell it is drawn into, and is lost if something else is drawn over that cell.
    pub shown: bool,
    bytes: usize,
    /// The frame it was last drawn in. A room picture keeps one for each size it is
    /// drawn at; one drawn in the current frame is on the screen and stays.
    drawn: u64,
}

/// Turns pixels into what the terminal draws. `None` if pictures are off or the
/// encoding failed.
fn encode(
    pixels: &DynamicImage,
    size: (u16, u16),
    graphics: Graphics,
    kitty_id: u32,
) -> Option<Encoded> {
    let cells = Size::new(size.0, size.1);
    match graphics.protocol? {
        Protocol::Kitty { compress } => {
            let font = FontSize::new(graphics.font.0, graphics.font.1);
            // Exactly as many pixels as the cells cover, padded with transparency.
            let fitted = Resize::Fit(Some(FilterType::Triangle)).resize(pixels, font, cells, None);
            let compress = compress && fitted.width() * fitted.height() > COMPRESS_ABOVE;
            let bytes = fitted.as_bytes().len() / 3 * 4;
            let kitty = Kitty::new(fitted, cells, kitty_id, false, compress).ok()?;
            Some(Encoded {
                size,
                protocol: SlicedProtocol::Kitty(kitty),
                kitty_id: Some(kitty_id),
                shown: false,
                bytes,
                drawn: 0,
            })
        }
        Protocol::Halfblocks => {
            let halfblocks = Halfblocks::new(pixels.clone(), cells).ok()?;
            Some(Encoded {
                size,
                protocol: SlicedProtocol::Halfblocks(halfblocks),
                kitty_id: None,
                // Half blocks are plain cells; there is nothing to transmit.
                shown: true,
                bytes: usize::from(size.0) * usize::from(size.1) * 16,
                drawn: 0,
            })
        }
    }
}

/// A room picture for `cells`, whichever size it is drawn at: cut to the shape of
/// those cells around its middle, so that it fills them without being squeezed.
/// `None` if pictures are not sent to the terminal, or for no cells at all.
fn encode_avatar(
    pixels: &DynamicImage,
    graphics: Graphics,
    kitty_id: u32,
    cells: (u16, u16),
) -> Option<Encoded> {
    let Some(Protocol::Kitty { .. }) = graphics.protocol else {
        return None;
    };
    let (cols, rows) = cells;
    if cols == 0 || rows == 0 {
        return None;
    }
    let width = u32::from(cols) * u32::from(graphics.font.0.max(1));
    let height = u32::from(rows) * u32::from(graphics.font.1.max(1));
    let (crop_width, crop_height) = cover_crop((pixels.width(), pixels.height()), (width, height));
    let fitted = pixels
        .crop_imm(
            (pixels.width() - crop_width) / 2,
            (pixels.height() - crop_height) / 2,
            crop_width,
            crop_height,
        )
        .resize_exact(width, height, FilterType::Triangle);
    let bytes = fitted.as_bytes().len() / 3 * 4;
    let kitty = Kitty::new(fitted, Size::new(cols, rows), kitty_id, false, false).ok()?;
    Some(Encoded {
        size: cells,
        protocol: SlicedProtocol::Kitty(kitty),
        kitty_id: Some(kitty_id),
        shown: false,
        bytes,
        drawn: 0,
    })
}

/// The largest part of a picture of `source` pixels that has the shape of `target`.
fn cover_crop(source: (u32, u32), target: (u32, u32)) -> (u32, u32) {
    let (width, height) = (u64::from(source.0.max(1)), u64::from(source.1.max(1)));
    let (target_width, target_height) = (u64::from(target.0.max(1)), u64::from(target.1.max(1)));
    if width * target_height > height * target_width {
        // Too wide: full height, part of the width.
        (
            ((height * target_width / target_height).max(1)) as u32,
            height as u32,
        )
    } else {
        (
            width as u32,
            ((width * target_height / target_width).max(1)) as u32,
        )
    }
}

/// A custom emoji for its cells: scaled to fit them, proportions kept, in the middle of
/// a transparent box of exactly their size. Nothing is cut off: a wide emoji is still
/// the whole emoji.
fn encode_emote(
    pixels: &DynamicImage,
    graphics: Graphics,
    kitty_id: u32,
    cells: (u16, u16),
) -> Option<Encoded> {
    let Some(Protocol::Kitty { .. }) = graphics.protocol else {
        return None;
    };
    let (cols, rows) = cells;
    let width = u32::from(cols.max(1)) * u32::from(graphics.font.0.max(1));
    let height = u32::from(rows.max(1)) * u32::from(graphics.font.1.max(1));
    let scaled = pixels
        .resize(width, height, FilterType::Triangle)
        .into_rgba8();
    let mut canvas = RgbaImage::new(width, height);
    image::imageops::overlay(
        &mut canvas,
        &scaled,
        i64::from(width.saturating_sub(scaled.width()) / 2),
        i64::from(height.saturating_sub(scaled.height()) / 2),
    );
    let fitted = DynamicImage::ImageRgba8(canvas);
    let bytes = fitted.as_bytes().len();
    let kitty = Kitty::new(fitted, Size::new(cols, rows), kitty_id, false, false).ok()?;
    Some(Encoded {
        size: cells,
        protocol: SlicedProtocol::Kitty(kitty),
        kitty_id: Some(kitty_id),
        shown: false,
        bytes,
        drawn: 0,
    })
}

/// Decodes a room picture and makes it a small square, cut from the middle.
pub fn decode_avatar(bytes: &[u8]) -> Result<Decoded, String> {
    // Larger than it is kept at, to leave enough of a non-square one for cutting.
    let decoded = decode(bytes, (AVATAR_PIXELS.0 * 4, AVATAR_PIXELS.1 * 4))?;
    let (width, height) = (decoded.pixels.width(), decoded.pixels.height());
    let side = width.min(height);
    let square = decoded
        .pixels
        .crop_imm((width - side) / 2, (height - side) / 2, side, side);
    let square = if side > AVATAR_PIXELS.0 {
        square.resize_exact(AVATAR_PIXELS.0, AVATAR_PIXELS.1, FilterType::Triangle)
    } else {
        square
    };
    Ok(Decoded {
        pixels: Arc::new(square),
        original: decoded.original,
    })
}

/// A picture the worker should fetch.
pub struct Request {
    pub key: String,
    pub media: Box<MediaRef>,
    /// File size as stated by the sender.
    pub size: Option<u64>,
    /// The whole file for the viewer, not a small version.
    pub full: bool,
}

enum State {
    Loading {
        since: Instant,
    },
    Failed(String),
    Ready {
        pixels: Pixels,
        encoded: Option<Encoded>,
    },
}

struct Entry {
    state: State,
    /// The frame this picture was last drawn or asked for in.
    used: u64,
}

impl Entry {
    fn bytes(&self) -> usize {
        match &self.state {
            State::Ready { pixels, encoded } => {
                pixels.as_bytes().len() + encoded.as_ref().map_or(0, |encoded| encoded.bytes)
            }
            State::Loading { .. } | State::Failed(_) => 0,
        }
    }
}

/// How a picture message is laid out in the timeline.
// Tests compare it with `assert_eq!`: there it shows everything, for a failure to be read.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, PartialEq, Eq)]
pub enum Slot {
    /// Pictures are off: the message shows its text instead.
    Off,
    /// One line saying why there is no picture.
    Failed(String),
    /// Columns and rows kept free for the picture.
    Cells(u16, u16),
}

/// Outside tests: why a picture failed can be an error's own words.
#[cfg(not(test))]
impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("Off"),
            Self::Failed(_) => f.write_str("Failed"),
            Self::Cells(columns, rows) => write!(f, "Cells({columns}, {rows})"),
        }
    }
}

enum ViewerState {
    Loading,
    Failed(String),
    Ready(Pixels),
}

/// The full-window view of one picture.
pub struct Viewer {
    pub info: ImageInfo,
    state: ViewerState,
    /// Pixel size of the file, once known.
    pub original: Option<(u32, u32)>,
    encoded: Option<Encoded>,
    /// The size an encoding is under way for on another thread.
    encoding: Option<(u16, u16)>,
    /// That encoding is to be thrown away when it arrives.
    encoding_stale: bool,
}

/// What the viewer has to show right now.
pub enum ViewerContent<'a> {
    Loading,
    /// The picture is being prepared for this window size.
    Preparing,
    Failed(&'a str),
    Ready(&'a mut Encoded),
}

/// A viewer encoding finished on another thread.
pub struct EncodeDone {
    key: String,
    size: (u16, u16),
    encoded: Option<Encoded>,
}

/// Which picture a pending transmission belongs to.
enum Target {
    Inline(String),
    Viewer,
    Avatar(String, (u16, u16)),
    Emote(String),
}

/// A small picture by its `mxc://` address: a room's or a person's picture, or a
/// custom emoji.
struct Small {
    state: SmallState,
    /// The frame it was last drawn in.
    used: u64,
}

impl Small {
    /// Asked for, and not given up on: the worker may still answer.
    fn on_its_way(&self) -> bool {
        matches!(self.state, SmallState::Loading { since } if since.elapsed() < REQUEST_TIMEOUT)
    }
}

enum SmallState {
    Loading {
        since: Instant,
    },
    /// The list shows the coloured tile instead (a custom emoji its name), without a
    /// word.
    Failed,
    Ready {
        pixels: Pixels,
        /// One for each size it is drawn at, the one drawn last at the end. A custom
        /// emoji has the one size; a room picture can have more.
        encoded: Vec<Encoded>,
    },
}

pub struct ImageStore {
    pub graphics: Graphics,
    entries: HashMap<String, Entry>,
    /// Sizes found by decoding, for pictures whose message did not state one. Kept even
    /// when the picture leaves the cache, so the layout never flips back.
    learned: HashMap<String, (u32, u32)>,
    requests: Vec<Request>,
    /// Counts frames, to know what was used recently.
    frame: u64,
    /// Changes whenever the space a picture takes in the timeline may have changed.
    pub layout_rev: u64,
    pub viewer: Option<Viewer>,
    spinner: usize,
    /// A loading indicator was drawn in the current frame.
    animating: bool,
    /// Transmissions put into the current frame: where their first cell is.
    unverified: Vec<(Target, u16, u16)>,
    /// Ids of pictures the terminal still holds but will not be asked to show again.
    garbage: Vec<u32>,
    next_id: u32,
    /// Where finished viewer encodings go. Without it they are done on the spot.
    encoder: Option<UnboundedSender<EncodeDone>>,
    /// Room pictures by their `mxc://` address. They are tiny and have their own
    /// limit, apart from the pictures in messages.
    avatars: HashMap<String, Small>,
    avatar_requests: Vec<String>,
    /// Custom emoji pictures by their `mxc://` address, with a limit of their own.
    emotes: HashMap<String, Small>,
    emote_requests: Vec<String>,
    /// Changes when a custom emoji's picture arrives or goes: the lines it is in are
    /// laid out again, with two cells for it or with its name.
    pub emote_rev: u64,
    /// Cells where a custom emoji's data was painted over by something drawn later in
    /// the frame: no copy there carries it in the next frame (see
    /// [`ImageStore::verify`]).
    emotes_painted_over: Vec<(u16, u16)>,
    /// A custom emoji's data was painted over: the frame is drawn again, the data
    /// travelling with another copy.
    redraw: bool,
}

impl std::fmt::Debug for ImageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageStore")
            .field("graphics", &self.graphics)
            .field("entries", &self.entries.len())
            .field("viewer", &self.viewer.is_some())
            .finish_non_exhaustive()
    }
}

impl ImageStore {
    pub fn new(graphics: Graphics) -> Self {
        // Ids start somewhere arbitrary so two instances in one terminal do not clash.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos())
            ^ std::process::id().rotate_left(16);
        Self {
            graphics,
            entries: HashMap::new(),
            learned: HashMap::new(),
            requests: Vec::new(),
            frame: 0,
            layout_rev: 0,
            viewer: None,
            spinner: 0,
            animating: false,
            unverified: Vec::new(),
            garbage: Vec::new(),
            next_id: seed,
            encoder: None,
            avatars: HashMap::new(),
            avatar_requests: Vec::new(),
            emotes: HashMap::new(),
            emote_requests: Vec::new(),
            emote_rev: 0,
            emotes_painted_over: Vec::new(),
            redraw: false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.graphics.protocol.is_some()
    }

    fn is_kitty(&self) -> bool {
        matches!(self.graphics.protocol, Some(Protocol::Kitty { .. }))
    }

    /// Viewer encodings are then done on a thread and delivered through `sender`.
    pub fn set_encoder(&mut self, sender: UnboundedSender<EncodeDone>) {
        self.encoder = Some(sender);
    }

    fn fresh_id(&mut self) -> u32 {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        self.next_id
    }

    fn discard(garbage: &mut Vec<u32>, encoded: impl IntoIterator<Item = Encoded>) {
        // Only what the terminal may have received needs deleting there, but an id it
        // never saw does no harm either.
        garbage.extend(encoded.into_iter().filter_map(|encoded| encoded.kitty_id));
    }

    // ----- layout --------------------------------------------------------------------

    /// How the picture of a message is laid out in a pane `width` columns wide.
    pub fn slot(&self, info: &ImageInfo, width: usize) -> Slot {
        if !self.enabled() {
            return Slot::Off;
        }
        if let Some(Entry {
            state: State::Failed(reason),
            ..
        }) = self.entries.get(&info.key)
        {
            return Slot::Failed(reason.clone());
        }
        let mut limit = inline_limit(width);
        if info.sticker {
            limit = (
                limit.0.min(STICKER_MAX_CELLS.0),
                limit.1.min(STICKER_MAX_CELLS.1),
            );
        }
        match info.dimensions.or(self.learned.get(&info.key).copied()) {
            Some(pixels) => {
                let (cols, rows) = fit_cells(pixels, self.graphics.font, limit);
                Slot::Cells(cols, rows)
            }
            None => Slot::Cells(UNKNOWN_CELLS.0.min(limit.0), UNKNOWN_CELLS.1.min(limit.1)),
        }
    }

    // ----- frames --------------------------------------------------------------------

    pub fn begin_frame(&mut self) {
        self.frame += 1;
        self.animating = false;
        self.unverified.clear();
    }

    /// Advances the loading indicator. True if one is on screen, so a redraw is due.
    pub fn tick(&mut self) -> bool {
        self.spinner = (self.spinner + 1) % SPINNER.len();
        self.animating
    }

    /// The current frame of the loading indicator; calling this means it is on screen.
    pub fn spinner(&mut self) -> &'static str {
        self.animating = true;
        SPINNER[self.spinner]
    }

    /// Makes sure the picture is in the cache or on its way. Called for every picture
    /// on screen or near it.
    pub fn request(&mut self, info: &ImageInfo) {
        if !self.enabled() {
            return;
        }
        let frame = self.frame;
        if let Some(entry) = self.entries.get_mut(&info.key) {
            entry.used = frame;
            match entry.state {
                State::Loading { since } if since.elapsed() >= REQUEST_TIMEOUT => {}
                State::Loading { .. } | State::Failed(_) | State::Ready { .. } => return,
            }
        }
        match &info.source {
            ImageSource::Demo => {
                let (width, height) = info.dimensions.unwrap_or(THUMBNAIL_PIXELS);
                let pixels = Arc::new(demo_picture(width, height));
                self.insert(
                    &info.key,
                    State::Ready {
                        pixels,
                        encoded: None,
                    },
                );
            }
            ImageSource::Matrix(media) => {
                self.insert(
                    &info.key,
                    State::Loading {
                        since: Instant::now(),
                    },
                );
                self.requests.push(Request {
                    key: info.key.clone(),
                    media: media.clone(),
                    size: info.size,
                    full: false,
                });
            }
        }
    }

    /// The requests made since the last call, for the worker.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    fn insert(&mut self, key: &str, state: State) {
        let entry = Entry {
            state,
            used: self.frame,
        };
        if let Some(Entry {
            state: State::Ready { encoded, .. },
            ..
        }) = self.entries.insert(key.to_owned(), entry)
        {
            Self::discard(&mut self.garbage, encoded);
        }
        self.evict();
    }

    /// Drops the pictures unused for longest until the cache is within its limits.
    /// What was used in the current frame always stays.
    fn evict(&mut self) {
        loop {
            let bytes: usize = self.entries.values().map(Entry::bytes).sum();
            if bytes <= CACHE_BYTES && self.entries.len() <= CACHE_ENTRIES {
                return;
            }
            let oldest = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.used < self.frame)
                .filter(|(_, entry)| !matches!(entry.state, State::Loading { .. }))
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone());
            let Some(key) = oldest else { return };
            match self.entries.remove(&key).map(|entry| entry.state) {
                Some(State::Ready { encoded, .. }) => Self::discard(&mut self.garbage, encoded),
                // Its one line becomes space for a picture again: lay out anew.
                Some(State::Failed(_)) => self.layout_rev += 1,
                _ => {}
            }
        }
    }

    /// Remembers the size found by decoding. Bounded: when full, only the sizes of
    /// pictures still in the cache are kept.
    fn learn(&mut self, key: &str, size: (u32, u32)) {
        if self.learned.len() >= LEARNED_ENTRIES && !self.learned.contains_key(key) {
            let entries = &self.entries;
            self.learned.retain(|key, _| entries.contains_key(key));
            self.layout_rev += 1;
        }
        self.learned.insert(key.to_owned(), size);
    }

    /// Forgets everything, for another account: the cache, the viewer, what was asked
    /// for. Results still on their way are then ignored. The terminal is told to free
    /// its copies with the next [`ImageStore::cleanup_sequence`].
    pub fn clear(&mut self) {
        self.close_viewer();
        self.forget_encoded();
        self.entries.clear();
        self.learned.clear();
        self.requests.clear();
        self.unverified.clear();
        self.avatars.clear();
        self.avatar_requests.clear();
        self.emotes.clear();
        self.emote_requests.clear();
        self.emotes_painted_over.clear();
        self.layout_rev += 1;
        self.emote_rev += 1;
    }

    // ----- room pictures -------------------------------------------------------------

    fn queue_avatar_if_needed(&mut self, url: &str) -> bool {
        let wanted = match self.avatars.get(url).map(|avatar| &avatar.state) {
            None => true,
            Some(SmallState::Loading { since }) => since.elapsed() >= REQUEST_TIMEOUT,
            _ => false,
        };
        if wanted {
            let state = SmallState::Loading {
                since: Instant::now(),
            };
            self.avatars.insert(
                url.to_owned(),
                Small {
                    state,
                    used: self.frame,
                },
            );
            self.avatar_requests.push(url.to_owned());
            self.evict_avatars();
        }
        wanted
    }

    /// Ensures `url` is queued for download (even on non-kitty terminals) and returns its
    /// decoded pixels immediately when already cached.
    pub fn request_avatar(&mut self, url: &str) -> Option<Pixels> {
        if self.queue_avatar_if_needed(url) {
            return None;
        }
        let avatar = self.avatars.get_mut(url)?;
        avatar.used = self.frame;
        match &avatar.state {
            SmallState::Ready { pixels, .. } => Some(Arc::clone(pixels)),
            SmallState::Loading { .. } | SmallState::Failed => None,
        }
    }

    /// The picture of a room or a person ready to draw into `cells` (columns and rows),
    /// if there is one yet. Asks for it otherwise. Only with real graphics: two cells of
    /// half blocks are four pixels. `covered` as for [`ImageStore::inline`].
    ///
    /// It is made for each size it is drawn at, and kept for each: the room list draws
    /// it four cells by two with room previews on, everything else two by one, and the
    /// two are on screen together or take turns in a narrow window.
    pub fn avatar(&mut self, url: &str, cells: (u16, u16), covered: bool) -> Option<&mut Encoded> {
        if !self.is_kitty() {
            return None;
        }
        let graphics = self.graphics;
        let frame = self.frame;
        if self.queue_avatar_if_needed(url) {
            return None;
        }
        let missing = matches!(
            &self.avatars.get(url)?.state,
            SmallState::Ready { encoded, .. } if !encoded.iter().any(|e| e.size == cells)
        );
        let id = if missing && !covered {
            self.fresh_id()
        } else {
            0
        };
        let avatar = self.avatars.get_mut(url)?;
        avatar.used = frame;
        let SmallState::Ready { pixels, encoded } = &mut avatar.state else {
            return None;
        };
        if missing {
            if covered {
                return None;
            }
            // Made before any other makes room: a size that cannot be made pushes
            // out none.
            encoded.push(encode_avatar(pixels, graphics, id, cells)?);
        }
        let at = encoded.iter().position(|e| e.size == cells)?;
        encoded[at].drawn = frame;
        encoded[at..].rotate_left(1);
        // Over the limit, the sizes drawn at longest ago make room, but none drawn in
        // this frame: that one is on the screen, and deleting it would blank it. As
        // they are in the order drawn, once the first was drawn in this frame all
        // were; then the picture keeps more for this frame, and a later frame that
        // draws fewer of them lets the rest go.
        while encoded.len() > AVATAR_SIZES && encoded[0].drawn != frame {
            Self::discard(&mut self.garbage, Some(encoded.remove(0)));
        }
        encoded
            .last_mut()
            .filter(|encoded| encoded.shown || !covered)
    }

    /// The addresses asked for since the last call, for the worker.
    pub fn take_avatar_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.avatar_requests)
    }

    /// The worker fetched (or failed to fetch) a room's picture.
    pub fn on_avatar(&mut self, url: &str, result: Result<Decoded, String>) {
        // Not asked for (any more), such as after logging out.
        let Some(avatar) = self.avatars.get_mut(url) else {
            return;
        };
        if !matches!(avatar.state, SmallState::Loading { .. }) {
            return;
        }
        avatar.state = match result {
            Ok(decoded) => SmallState::Ready {
                pixels: decoded.pixels,
                encoded: Vec::new(),
            },
            Err(_) => SmallState::Failed,
        };
    }

    /// Remembers that the untransmitted picture for `cells` was put into the frame at
    /// this cell.
    pub fn sent_avatar(&mut self, url: &str, cells: (u16, u16), x: u16, y: u16) {
        self.unverified
            .push((Target::Avatar(url.to_owned(), cells), x, y));
    }

    /// Keeps the room pictures within their limit, dropping those unseen for longest.
    fn evict_avatars(&mut self) {
        while self.avatars.len() > AVATAR_ENTRIES {
            let oldest = self
                .avatars
                .iter()
                .filter(|(_, avatar)| avatar.used < self.frame)
                .min_by_key(|(_, avatar)| avatar.used)
                .map(|(url, _)| url.clone());
            let Some(url) = oldest else { return };
            if let Some(Small {
                state: SmallState::Ready { encoded, .. },
                ..
            }) = self.avatars.remove(&url)
            {
                Self::discard(&mut self.garbage, encoded);
            }
        }
    }

    // ----- custom emoji ------------------------------------------------------------

    /// Whether custom emoji are drawn as pictures at all: only with kitty's protocol.
    /// Two cells of half blocks would be four pixels; everywhere else they keep their
    /// names, and nothing is fetched for them.
    pub fn emote_pictures(&self) -> bool {
        self.is_kitty()
    }

    /// Whether the picture of a custom emoji is there to be drawn. Lines are laid out
    /// with two cells for it then, and with its name until then.
    pub fn emote_ready(&self, url: &str) -> bool {
        self.is_kitty()
            && matches!(
                self.emotes.get(url),
                Some(Small {
                    state: SmallState::Ready { .. },
                    ..
                })
            )
    }

    /// Makes sure the pictures of the custom emoji that are on screen are in the cache
    /// or on their way. Called on every frame with every one shown; only
    /// [`EMOTE_IN_FLIGHT`] are fetched at a time, the others are asked for again in
    /// later frames.
    pub fn request_emotes<'a>(&mut self, urls: impl IntoIterator<Item = &'a str>) {
        if !self.is_kitty() {
            return;
        }
        let frame = self.frame;
        let mut loading = self
            .emotes
            .values()
            .filter(|small| small.on_its_way())
            .count();
        for url in urls {
            if let Some(small) = self.emotes.get_mut(url) {
                small.used = frame;
                // One the worker never answered (it restarted) is asked for again.
                if let SmallState::Loading { since } = &mut small.state
                    && since.elapsed() >= REQUEST_TIMEOUT
                    && loading < EMOTE_IN_FLIGHT
                {
                    *since = Instant::now();
                    loading += 1;
                    self.emote_requests.push(url.to_owned());
                }
                continue;
            }
            if loading >= EMOTE_IN_FLIGHT || !crate::matrix::is_valid_mxc(url) {
                continue;
            }
            loading += 1;
            self.emotes.insert(
                url.to_owned(),
                Small {
                    state: SmallState::Loading {
                        since: Instant::now(),
                    },
                    used: frame,
                },
            );
            self.emote_requests.push(url.to_owned());
            self.evict_emotes();
        }
    }

    /// [`ImageStore::request_emotes`] for one.
    #[cfg(test)]
    pub fn request_emote(&mut self, url: &str) {
        self.request_emotes([url]);
    }

    /// The addresses of custom emoji asked for since the last call, for the worker.
    pub fn take_emote_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.emote_requests)
    }

    /// The worker fetched (or failed to fetch) a custom emoji's picture.
    pub fn on_emote(&mut self, url: &str, result: Result<Decoded, String>) {
        // Not asked for (any more), such as after logging out.
        let Some(small) = self.emotes.get_mut(url) else {
            return;
        };
        if !matches!(small.state, SmallState::Loading { .. }) {
            return;
        }
        small.state = match result {
            Ok(decoded) => {
                // Two cells instead of the name: the lines are laid out again.
                self.emote_rev += 1;
                SmallState::Ready {
                    pixels: decoded.pixels,
                    encoded: Vec::new(),
                }
            }
            // The name stays where it is.
            Err(_) => SmallState::Failed,
        };
    }

    /// A custom emoji's picture ready to draw into its two cells, if it is there.
    /// `covered` as for [`ImageStore::inline`].
    pub fn emote(&mut self, url: &str, covered: bool) -> Option<&mut Encoded> {
        if !self.is_kitty() {
            return None;
        }
        let graphics = self.graphics;
        let frame = self.frame;
        let missing = matches!(
            &self.emotes.get(url)?.state,
            SmallState::Ready { encoded, .. } if encoded.is_empty()
        );
        let id = if missing && !covered {
            self.fresh_id()
        } else {
            0
        };
        let small = self.emotes.get_mut(url)?;
        small.used = frame;
        let SmallState::Ready { pixels, encoded } = &mut small.state else {
            return None;
        };
        if missing {
            if covered {
                return None;
            }
            encoded.extend(encode_emote(pixels, graphics, id, EMOTE_CELLS));
        }
        encoded
            .first_mut()
            .filter(|encoded| encoded.shown || !covered)
    }

    pub fn sent_emote(&mut self, url: &str, x: u16, y: u16) {
        self.unverified.push((Target::Emote(url.to_owned()), x, y));
    }

    /// Whether a custom emoji's data was painted over at this cell in the last frame,
    /// so that a copy drawn there must not carry it again.
    pub fn emote_painted_over(&self, x: u16, y: u16) -> bool {
        self.emotes_painted_over.contains(&(x, y))
    }

    /// Whether the frame just drawn has to be drawn again (see [`ImageStore::verify`]).
    pub fn take_redraw(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// Keeps the custom emoji pictures within their limit, dropping those unseen for
    /// longest. One on its way stays, or more than [`EMOTE_IN_FLIGHT`] could be.
    fn evict_emotes(&mut self) {
        while self.emotes.len() > EMOTE_ENTRIES {
            let oldest = self
                .emotes
                .iter()
                .filter(|(_, small)| small.used < self.frame)
                .filter(|(_, small)| !matches!(small.state, SmallState::Loading { .. }))
                .min_by_key(|(_, small)| small.used)
                .map(|(url, _)| url.clone());
            let Some(url) = oldest else { return };
            if let Some(Small {
                state: SmallState::Ready { encoded, .. },
                ..
            }) = self.emotes.remove(&url)
            {
                Self::discard(&mut self.garbage, encoded);
                // Its lines show its name again.
                self.emote_rev += 1;
            }
        }
    }

    /// The worker fetched (or failed to fetch) a picture.
    pub fn on_loaded(&mut self, key: &str, full: bool, result: Result<Decoded, String>) {
        if full {
            self.on_full_loaded(key, result);
            return;
        }
        // Not asked for (any more): nothing is waiting for it.
        if !matches!(
            self.entries.get(key),
            Some(Entry {
                state: State::Loading { .. },
                ..
            })
        ) {
            return;
        }
        self.layout_rev += 1;
        match result {
            Ok(decoded) => {
                self.learn(key, decoded.original);
                self.insert(
                    key,
                    State::Ready {
                        pixels: decoded.pixels,
                        encoded: None,
                    },
                );
            }
            Err(reason) => self.insert(key, State::Failed(reason)),
        }
    }

    fn on_full_loaded(&mut self, key: &str, result: Result<Decoded, String>) {
        let Some(viewer) = self.viewer.as_mut().filter(|viewer| viewer.info.key == key) else {
            return;
        };
        match result {
            Ok(decoded) => {
                viewer.original = Some(decoded.original);
                viewer.state = ViewerState::Ready(decoded.pixels.clone());
                // A picture that failed between the messages gets a second chance.
                if matches!(
                    self.entries.get(key),
                    Some(Entry {
                        state: State::Failed(_),
                        ..
                    })
                ) {
                    let (width, height) = THUMBNAIL_PIXELS;
                    let small = Arc::new(decoded.pixels.thumbnail(width, height));
                    self.learn(key, decoded.original);
                    self.layout_rev += 1;
                    self.insert(
                        key,
                        State::Ready {
                            pixels: small,
                            encoded: None,
                        },
                    );
                }
            }
            Err(reason) => viewer.state = ViewerState::Failed(reason),
        }
    }

    /// Why the picture of a message could not be loaded, if it could not.
    #[cfg(test)]
    pub fn failure(&self, key: &str) -> Option<&str> {
        match &self.entries.get(key)?.state {
            State::Failed(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn is_loading(&self, key: &str) -> bool {
        matches!(
            self.entries.get(key),
            Some(Entry {
                state: State::Loading { .. },
                ..
            })
        )
    }

    /// The picture ready to draw at `size` cells, encoding it first if need be.
    /// `covered`: something may be drawn on top in this frame, so nothing new may be
    /// transmitted now (see [`Encoded::shown`]).
    pub fn inline(&mut self, key: &str, size: (u16, u16), covered: bool) -> Option<&mut Encoded> {
        let graphics = self.graphics;
        let hold = covered && self.is_kitty();
        let frame = self.frame;
        let stale = match &self.entries.get(key)?.state {
            State::Ready { encoded, .. } => encoded.as_ref().is_none_or(|e| e.size != size),
            State::Loading { .. } | State::Failed(_) => return None,
        };
        let id = if stale && !hold { self.fresh_id() } else { 0 };
        let entry = self.entries.get_mut(key)?;
        entry.used = frame;
        if stale {
            if hold {
                return None;
            }
            if let State::Ready { pixels, encoded } = &mut entry.state {
                Self::discard(&mut self.garbage, encoded.take());
                *encoded = encode(pixels, size, graphics, id);
            }
            // The encoding counts towards the cache's size. This picture stays: it
            // was used in this frame.
            self.evict();
        }
        match &mut self.entries.get_mut(key)?.state {
            State::Ready { encoded, .. } => {
                encoded.as_mut().filter(|encoded| encoded.shown || !hold)
            }
            State::Loading { .. } | State::Failed(_) => None,
        }
    }

    /// Remembers that an untransmitted picture was put into the frame at this cell.
    pub fn sent_inline(&mut self, key: &str, x: u16, y: u16) {
        self.unverified.push((Target::Inline(key.to_owned()), x, y));
    }

    pub fn sent_viewer(&mut self, x: u16, y: u16) {
        self.unverified.push((Target::Viewer, x, y));
    }

    /// Called with the finished frame: checks that every transmission is still in the
    /// cell it was put into. One that was painted over never reaches the terminal, so
    /// that picture is encoded again for the next frame.
    ///
    /// Only that one cell matters: the transmission is the whole picture, and the
    /// other cells are plain placeholders that may be covered freely. Whatever is drawn
    /// over the timeline later in a frame either sets `covered` (popups, the viewer,
    /// the timeline's pills and the completion popup), which holds new transmissions
    /// back, or is caught here.
    ///
    /// A custom emoji is often on screen more than once, the other copies referring to
    /// the one that carries the data. When that one is painted over, they would stay
    /// empty: the frame is drawn again, and no copy in that cell carries the data then.
    /// The cells are forgotten with the first frame that loses no custom emoji's data:
    /// by then it has arrived with another copy, if one is on screen, and a copy under
    /// whatever is drawn over it only refers to that.
    pub fn verify(&mut self, intact: impl Fn(u16, u16) -> bool) {
        let mut emote_lost = false;
        for (target, x, y) in std::mem::take(&mut self.unverified) {
            // Whether the terminal got it.
            let kept = intact(x, y);
            match &target {
                Target::Inline(key) => {
                    if let Some(Entry {
                        state: State::Ready { encoded, .. },
                        ..
                    }) = self.entries.get_mut(key)
                    {
                        Self::settle(&mut self.garbage, encoded, kept);
                    }
                }
                Target::Viewer => {
                    if let Some(viewer) = self.viewer.as_mut() {
                        Self::settle(&mut self.garbage, &mut viewer.encoded, kept);
                    }
                }
                Target::Avatar(url, cells) => {
                    if let Some(Small {
                        state: SmallState::Ready { encoded, .. },
                        ..
                    }) = self.avatars.get_mut(url)
                    {
                        Self::settle_one(&mut self.garbage, encoded, *cells, kept);
                    }
                }
                Target::Emote(url) => {
                    if let Some(Small {
                        state: SmallState::Ready { encoded, .. },
                        ..
                    }) = self.emotes.get_mut(url)
                    {
                        Self::settle_one(&mut self.garbage, encoded, EMOTE_CELLS, kept);
                        if !kept {
                            emote_lost = true;
                            self.emotes_painted_over.push((x, y));
                        }
                    }
                }
            }
        }
        if emote_lost {
            self.redraw = true;
        } else {
            self.emotes_painted_over.clear();
        }
    }

    /// After a frame: the terminal has `encoded` if its transmission was `kept`;
    /// otherwise it never got it, and it is made again when next drawn.
    fn settle(garbage: &mut Vec<u32>, encoded: &mut Option<Encoded>, kept: bool) {
        if !kept {
            Self::discard(garbage, encoded.take());
        } else if let Some(encoded) = encoded {
            encoded.shown = true;
        }
    }

    /// [`ImageStore::settle`] for the one of a small picture's encodings that is made
    /// for `cells`.
    fn settle_one(
        garbage: &mut Vec<u32>,
        encoded: &mut Vec<Encoded>,
        cells: (u16, u16),
        kept: bool,
    ) {
        let Some(at) = encoded.iter().position(|encoded| encoded.size == cells) else {
            return;
        };
        if kept {
            encoded[at].shown = true;
        } else {
            Self::discard(garbage, Some(encoded.remove(at)));
        }
    }

    /// The terminal lost what it was sent (it was cleared or resized): everything is
    /// transmitted again when it is next drawn.
    pub fn forget_encoded(&mut self) {
        for entry in self.entries.values_mut() {
            if let State::Ready { encoded, .. } = &mut entry.state {
                Self::discard(&mut self.garbage, encoded.take());
            }
        }
        for small in self.avatars.values_mut().chain(self.emotes.values_mut()) {
            if let SmallState::Ready { encoded, .. } = &mut small.state {
                Self::discard(&mut self.garbage, std::mem::take(encoded));
            }
        }
        if let Some(viewer) = &mut self.viewer {
            Self::discard(&mut self.garbage, viewer.encoded.take());
            // An encoding still under way is not to be used when it arrives.
            viewer.encoding_stale = viewer.encoding.is_some();
        }
    }

    /// Escape sequences that make the terminal free the pictures no longer needed
    /// (`all`: every picture, when quitting). Empty unless the kitty protocol is used.
    pub fn cleanup_sequence(&mut self, all: bool) -> String {
        if all {
            self.forget_encoded();
        }
        let ids = std::mem::take(&mut self.garbage);
        if !self.is_kitty() {
            return String::new();
        }
        ids.iter()
            .map(|id| format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\"))
            .collect()
    }

    // ----- viewer --------------------------------------------------------------------

    /// The picture the viewer is showing.
    pub fn viewer_info(&self) -> Option<&ImageInfo> {
        self.viewer.as_ref().map(|viewer| &viewer.info)
    }

    pub fn viewer_open(&self) -> bool {
        self.viewer.is_some()
    }

    /// Opens the full-window view and asks for the whole file.
    pub fn open_viewer(&mut self, info: &ImageInfo) {
        self.close_viewer();
        let mut viewer = Viewer {
            info: info.clone(),
            state: ViewerState::Loading,
            original: info.dimensions,
            encoded: None,
            encoding: None,
            encoding_stale: false,
        };
        match &info.source {
            ImageSource::Demo => {
                // The demo picture is drawn at any size; make it worth a large window.
                let key = info.key.as_str();
                debug_assert_eq!(key, DEMO_IMAGE);
                viewer.state = ViewerState::Ready(Arc::new(demo_picture(1280, 720)));
                viewer.original = Some((1280, 720));
            }
            ImageSource::Matrix(media) => self.requests.push(Request {
                key: info.key.clone(),
                media: media.clone(),
                size: info.size,
                full: true,
            }),
        }
        self.viewer = Some(viewer);
    }

    pub fn close_viewer(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            Self::discard(&mut self.garbage, viewer.encoded);
        }
    }

    /// The pixel size of the picture in the viewer, once it is loaded.
    pub fn viewer_pixels(&self) -> Option<(u32, u32)> {
        match &self.viewer.as_ref()?.state {
            ViewerState::Ready(pixels) => Some((pixels.width(), pixels.height())),
            ViewerState::Loading | ViewerState::Failed(_) => None,
        }
    }

    /// What the viewer shows at `size` cells. Starts the encoding for that size if it
    /// is missing; `covered` as for [`ImageStore::inline`].
    pub fn viewer_content(&mut self, size: (u16, u16), covered: bool) -> ViewerContent<'_> {
        let graphics = self.graphics;
        let hold = covered && self.is_kitty();
        let needs_encoding = match self.viewer.as_ref() {
            Some(Viewer {
                state: ViewerState::Ready(_),
                encoded,
                encoding,
                ..
            }) => {
                // One encoding at a time: while a window is dragged through many
                // sizes, the next starts when the running one is done.
                encoded.as_ref().is_none_or(|e| e.size != size) && encoding.is_none() && !hold
            }
            _ => false,
        };
        let id = if needs_encoding { self.fresh_id() } else { 0 };
        let encoder = self.encoder.clone();
        let Some(viewer) = self.viewer.as_mut() else {
            return ViewerContent::Loading;
        };
        let pixels = match &viewer.state {
            ViewerState::Loading => return ViewerContent::Loading,
            ViewerState::Failed(reason) => return ViewerContent::Failed(reason),
            ViewerState::Ready(pixels) => pixels.clone(),
        };
        if needs_encoding {
            Self::discard(&mut self.garbage, viewer.encoded.take());
            match encoder {
                Some(sender) => {
                    viewer.encoding = Some(size);
                    let key = viewer.info.key.clone();
                    std::thread::spawn(move || {
                        let encoded = encode(&pixels, size, graphics, id);
                        let _ = sender.send(EncodeDone { key, size, encoded });
                    });
                }
                None => viewer.encoded = encode(&pixels, size, graphics, id),
            }
        }
        match viewer.encoded.as_mut() {
            Some(encoded) if encoded.size == size && (encoded.shown || !hold) => {
                ViewerContent::Ready(encoded)
            }
            _ => ViewerContent::Preparing,
        }
    }

    /// An encoding for the viewer arrived from its thread.
    pub fn on_encoded(&mut self, done: EncodeDone) {
        match self.viewer.as_mut() {
            Some(viewer) if viewer.info.key == done.key && viewer.encoding == Some(done.size) => {
                viewer.encoding = None;
                if std::mem::take(&mut viewer.encoding_stale) {
                    // Made before the terminal forgot its pictures; the next draw
                    // starts over.
                    Self::discard(&mut self.garbage, done.encoded);
                    return;
                }
                Self::discard(&mut self.garbage, viewer.encoded.take());
                viewer.encoded = done.encoded;
                if viewer.encoded.is_none() {
                    viewer.state = ViewerState::Failed("could not be prepared".to_owned());
                }
            }
            // For a viewer that is closed by now.
            _ => Self::discard(&mut self.garbage, done.encoded),
        }
    }
}

/// "1.2 MB", for titles.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "kB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::model::{ImageInfo, ImageSource, MediaRef};
    use matrix_sdk::ruma::OwnedMxcUri;
    use matrix_sdk::ruma::events::room::MediaSource;

    /// A picture message's info pointing at a made-up Matrix file.
    pub fn remote_image(key: &str, dimensions: Option<(u32, u32)>) -> ImageInfo {
        ImageInfo {
            key: key.to_owned(),
            source: ImageSource::Matrix(Box::new(MediaRef {
                full: MediaSource::Plain(OwnedMxcUri::from(key)),
                thumbnail: None,
            })),
            name: "cat.png".to_owned(),
            caption: None,
            dimensions,
            size: Some(2048),
            sticker: false,
        }
    }

    pub fn decoded(width: u32, height: u32) -> Decoded {
        Decoded {
            pixels: Arc::new(demo_picture(width, height)),
            original: (width, height),
        }
    }

    pub fn kitty() -> Graphics {
        Graphics {
            protocol: Some(Protocol::Kitty { compress: false }),
            font: (10, 20),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn pictures_fit_their_box_and_keep_their_shape() {
        let font = (10, 20);
        // Smaller than the box: natural size, never scaled up.
        assert_eq!(fit_cells((320, 200), font, (40, 12)), (32, 10));
        assert_eq!(fit_cells((16, 16), font, (40, 12)), (2, 1));
        // Landscape: limited by the width or by the height, whichever comes first.
        assert_eq!(fit_cells((4000, 1000), font, (40, 12)), (40, 5));
        assert_eq!(fit_cells((800, 600), font, (40, 12)), (32, 12));
        // Portrait: limited by the height.
        assert_eq!(fit_cells((600, 2400), font, (40, 12)), (6, 12));
        // Extreme shapes still get at least one cell each way.
        assert_eq!(fit_cells((10_000, 10), font, (40, 12)), (40, 1));
        assert_eq!(fit_cells((1, 100_000), font, (40, 12)), (1, 12));
        // Nonsense input does not panic or return nothing.
        assert_eq!(fit_cells((0, 0), (0, 0), (0, 0)), (1, 1));
        // A different font changes the cell count, not the shape on screen.
        assert_eq!(fit_cells((320, 200), (8, 16), (40, 12)), (39, 12));
        assert_eq!(fit_cells((320, 320), (8, 16), (40, 12)), (24, 12));
    }

    #[test]
    fn narrow_panes_get_smaller_pictures() {
        assert_eq!(inline_limit(100), (40, 12));
        assert_eq!(inline_limit(40), (40, 12));
        assert_eq!(inline_limit(30), (30, 10));
        assert_eq!(inline_limit(12), (12, 4));
        assert_eq!(inline_limit(0), (1, 3));
    }

    #[test]
    fn decoding_scales_down_and_refuses_bombs_and_junk() {
        let mut png = Vec::new();
        demo_picture(1000, 500)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let decoded = decode(&png, THUMBNAIL_PIXELS).unwrap();
        assert_eq!(decoded.original, (1000, 500));
        assert_eq!(
            (decoded.pixels.width(), decoded.pixels.height()),
            (640, 320)
        );
        // Small enough: left as it is.
        let decoded = decode(&png, FULL_PIXELS).unwrap();
        assert_eq!(
            (decoded.pixels.width(), decoded.pixels.height()),
            (1000, 500)
        );

        assert!(decode(b"not a picture at all", THUMBNAIL_PIXELS).is_err());
        assert!(decode(&png[..png.len() / 2], THUMBNAIL_PIXELS).is_err());
        assert!(decode(&[], THUMBNAIL_PIXELS).is_err());

        // A photo taken with the camera on its side is turned upright: the file is
        // 40x20 with a note saying "rotate by 90 degrees".
        let mut jpeg = Vec::new();
        demo_picture(40, 20)
            .to_rgb8()
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();
        let mut exif = b"\xff\xe1\x00\x22Exif\0\0II*\0\x08\0\0\0\x01\0".to_vec();
        exif.extend(b"\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0");
        let mut turned = jpeg[..2].to_vec();
        turned.extend(&exif);
        turned.extend(&jpeg[2..]);
        assert_eq!(decode(&jpeg, FULL_PIXELS).unwrap().original, (40, 20));
        assert_eq!(decode(&turned, FULL_PIXELS).unwrap().original, (20, 40));

        // A grey picture needs one byte per pixel in the file's own format but four
        // once it is turned or scaled: 16384x4100 passes as 67 MB of grey and must
        // still be refused. (Only the header is real; nothing is read beyond it.)
        let mut grey = Vec::new();
        DynamicImage::new_luma8(4, 4)
            .write_to(&mut Cursor::new(&mut grey), image::ImageFormat::Png)
            .unwrap();
        grey[16..20].copy_from_slice(&16_384u32.to_be_bytes());
        grey[20..24].copy_from_slice(&4_100u32.to_be_bytes());
        let crc = crc32(&grey[12..29]);
        grey[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(
            decode(&grey, THUMBNAIL_PIXELS).err().as_deref(),
            Some("too large to show")
        );
        // The same header with a size that is allowed gets past that check (and then
        // fails on the missing pixels), so it is the size that was refused above.
        grey[16..20].copy_from_slice(&1_000u32.to_be_bytes());
        let crc = crc32(&grey[12..29]);
        grey[29..33].copy_from_slice(&crc.to_be_bytes());
        assert_eq!(
            decode(&grey, THUMBNAIL_PIXELS).err().as_deref(),
            Some("the file is damaged")
        );

        // A small file that unpacks into something enormous is refused.
        let mut bomb = Vec::new();
        DynamicImage::new_luma8(MAX_DECODE_SIDE + 1, 1)
            .write_to(&mut Cursor::new(&mut bomb), image::ImageFormat::Png)
            .unwrap();
        assert!(bomb.len() < 1000);
        assert_eq!(
            decode(&bomb, THUMBNAIL_PIXELS).err().as_deref(),
            Some("too large to show")
        );
    }

    /// One chunk of a PNG file: its length, its type and data, and their checksum.
    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        chunk.extend(kind);
        chunk.extend(data);
        let crc = crc32(&chunk[4..]);
        chunk.extend(crc.to_be_bytes());
        chunk
    }

    /// The chunks of a PNG file as (type, data).
    fn png_chunks(file: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let mut chunks = Vec::new();
        let mut at = 8;
        while at + 8 <= file.len() {
            let length = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
            let kind: [u8; 4] = file[at + 4..at + 8].try_into().unwrap();
            chunks.push((kind, file[at + 8..at + 8 + length].to_vec()));
            at += 12 + length;
        }
        chunks
    }

    const FIRST: Rgba<u8> = Rgba([220, 30, 30, 255]);
    const SECOND: Rgba<u8> = Rgba([30, 30, 220, 255]);
    const ANIMATED: (u32, u32) = (8, 6);

    /// Whether `pixel` is `colour`, give or take a shade: a GIF keeps its colours in a
    /// table, and an animated WebP rounds them when a frame is laid on its canvas.
    fn same_colour(pixel: [u8; 4], colour: Rgba<u8>) -> bool {
        pixel
            .iter()
            .zip(colour.0)
            .all(|(got, want)| got.abs_diff(want) <= 12)
    }

    /// A GIF of two frames: the first all `FIRST`, the second all `SECOND`.
    fn animated_gif() -> Vec<u8> {
        let mut gif = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut gif);
            encoder
                .encode_frames([FIRST, SECOND].map(|colour| {
                    image::Frame::new(RgbaImage::from_pixel(ANIMATED.0, ANIMATED.1, colour))
                }))
                .unwrap();
        }
        gif
    }

    /// The same as an animated PNG: the picture of a plain PNG is the first frame, the
    /// second one follows in the chunks that only an animation has.
    fn animated_png() -> Vec<u8> {
        let still = |colour| {
            let mut png = Vec::new();
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(ANIMATED.0, ANIMATED.1, colour))
                .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            png
        };
        let (first, second) = (still(FIRST), still(SECOND));
        let frame_control = |sequence: u32| {
            let mut data = sequence.to_be_bytes().to_vec();
            data.extend(ANIMATED.0.to_be_bytes());
            data.extend(ANIMATED.1.to_be_bytes());
            // At the top left corner, a tenth of a second, drawn over what was there.
            data.extend([0u8; 8]);
            data.extend(1u16.to_be_bytes());
            data.extend(10u16.to_be_bytes());
            data.extend([0u8, 0]);
            png_chunk(b"fcTL", &data)
        };
        let mut apng = first[..8].to_vec();
        for (kind, data) in png_chunks(&first) {
            match &kind {
                b"IHDR" => {
                    apng.extend(png_chunk(&kind, &data));
                    // Two frames, played for ever.
                    apng.extend(png_chunk(b"acTL", &[0, 0, 0, 2, 0, 0, 0, 0]));
                    apng.extend(frame_control(0));
                }
                b"IEND" => {}
                _ => apng.extend(png_chunk(&kind, &data)),
            }
        }
        apng.extend(frame_control(1));
        let mut sequence = 2u32;
        for (kind, data) in png_chunks(&second) {
            if &kind == b"IDAT" {
                let mut frame = sequence.to_be_bytes().to_vec();
                frame.extend(data);
                apng.extend(png_chunk(b"fdAT", &frame));
                sequence += 1;
            }
        }
        apng.extend(png_chunk(b"IEND", &[]));
        apng
    }

    /// One chunk of a WebP file: its type, its length and its data, padded to an even
    /// length.
    fn riff_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = kind.to_vec();
        chunk.extend((data.len() as u32).to_le_bytes());
        chunk.extend(data);
        if !data.len().is_multiple_of(2) {
            chunk.push(0);
        }
        chunk
    }

    /// The same as an animated WebP: two frames that each fill the canvas.
    fn animated_webp() -> Vec<u8> {
        // The picture data of a still WebP of one colour, as the frames hold it.
        let still = |colour| {
            let mut webp = Vec::new();
            let pixels = RgbaImage::from_pixel(ANIMATED.0, ANIMATED.1, colour);
            image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
                .encode(
                    pixels.as_raw(),
                    ANIMATED.0,
                    ANIMATED.1,
                    image::ExtendedColorType::Rgba8,
                )
                .unwrap();
            assert_eq!(&webp[12..16], b"VP8L");
            let length = u32::from_le_bytes(webp[16..20].try_into().unwrap()) as usize;
            webp[20..20 + length].to_vec()
        };
        let three = |value: u32| value.to_le_bytes()[..3].to_vec();
        let frame = |colour| {
            // At the top left corner, as large as the canvas, a tenth of a second.
            let mut data = [three(0), three(0)].concat();
            data.extend(three(ANIMATED.0 - 1));
            data.extend(three(ANIMATED.1 - 1));
            data.extend(three(100));
            data.push(0);
            data.extend(riff_chunk(b"VP8L", &still(colour)));
            riff_chunk(b"ANMF", &data)
        };
        // Animated, with transparency; then the size of the canvas.
        let mut header = vec![0b0001_0010, 0, 0, 0];
        header.extend(three(ANIMATED.0 - 1));
        header.extend(three(ANIMATED.1 - 1));
        let mut body = b"WEBP".to_vec();
        body.extend(riff_chunk(b"VP8X", &header));
        // No background colour, played for ever.
        body.extend(riff_chunk(b"ANIM", &[0; 6]));
        body.extend(frame(FIRST));
        body.extend(frame(SECOND));
        let mut webp = b"RIFF".to_vec();
        webp.extend((body.len() as u32).to_le_bytes());
        webp.extend(body);
        webp
    }

    /// An animated sticker (GIF, animated PNG, animated WebP) is shown as a still
    /// picture: its first frame. (That the later frames are not read is shown for the
    /// GIF only, by cutting it off inside its second frame.)
    #[test]
    fn an_animated_picture_is_shown_as_its_first_frame() {
        use image::AnimationDecoder;

        // What is tested are animations: each file holds two frames.
        let gif = animated_gif();
        let frames = image::codecs::gif::GifDecoder::new(Cursor::new(&gif))
            .unwrap()
            .into_frames()
            .count();
        assert_eq!(frames, 2);
        let png = animated_png();
        let frames = image::codecs::png::PngDecoder::new(Cursor::new(&png))
            .unwrap()
            .apng()
            .unwrap()
            .into_frames()
            .map(|frame| frame.unwrap().into_buffer().get_pixel(0, 0).0)
            .collect::<Vec<_>>();
        assert_eq!(frames, [FIRST.0, SECOND.0]);
        let webp = animated_webp();
        let decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(&webp)).unwrap();
        assert!(decoder.has_animation());
        let frames = decoder
            .into_frames()
            .map(|frame| frame.unwrap().into_buffer().get_pixel(0, 0).0)
            .collect::<Vec<_>>();
        // Laying a frame on the canvas rounds its colours: a shade off is the same.
        assert_eq!(frames.len(), 2);
        assert!(same_colour(frames[0], FIRST) && same_colour(frames[1], SECOND));

        for (kind, file) in [("gif", &gif), ("png", &png), ("webp", &webp)] {
            let decoded =
                decode(file, THUMBNAIL_PIXELS).unwrap_or_else(|why| panic!("{kind}: {why}"));
            assert_eq!(decoded.original, ANIMATED, "{kind}");
            let shown = decoded.pixels.to_rgba8();
            for (x, y) in [(0, 0), (ANIMATED.0 - 1, ANIMATED.1 - 1)] {
                let pixel = shown.get_pixel(x, y).0;
                let close = same_colour(pixel, FIRST);
                assert!(close, "{kind} at {x},{y}: {pixel:?}");
            }
        }

        // A GIF cut off inside its second frame still shows its first: the rest of
        // the file is not looked at.
        let cut = &gif[..gif.len() - 12];
        assert_eq!(decode(cut, THUMBNAIL_PIXELS).unwrap().original, ANIMATED);
    }

    /// The canvas of an animation is what has to fit into memory, however small its
    /// frames and its file are.
    #[test]
    fn an_animation_with_an_enormous_canvas_is_refused() {
        // A GIF says its canvas size in bytes 6 to 9.
        let mut gif = animated_gif();
        gif[6..8].copy_from_slice(&16_000u16.to_le_bytes());
        gif[8..10].copy_from_slice(&16_000u16.to_le_bytes());
        assert_eq!(
            decode(&gif, THUMBNAIL_PIXELS).err().as_deref(),
            Some("too large to show")
        );
        // A WebP, in its header chunk: one less than the width and the height, in
        // three bytes each, after the RIFF header (12), the chunk's own (8) and the
        // flags (4).
        let mut webp = animated_webp();
        webp[24..27].copy_from_slice(&16_383u32.to_le_bytes()[..3]);
        webp[27..30].copy_from_slice(&16_383u32.to_le_bytes()[..3]);
        assert_eq!(
            decode(&webp, THUMBNAIL_PIXELS).err().as_deref(),
            Some("too large to show")
        );
        assert!(gif.len() < 1000 && webp.len() < 1000);
    }

    #[test]
    fn a_sticker_gets_less_room_than_a_photo() {
        let store = ImageStore::new(Graphics::halfblocks());
        let mut sticker = remote_image("mxc://x/sticker", Some((512, 512)));
        let photo = sticker.clone();
        sticker.sticker = true;
        assert_eq!(store.slot(&photo, 80), Slot::Cells(24, 12));
        assert_eq!(store.slot(&sticker, 80), Slot::Cells(14, 7));
        // A small one is not blown up.
        sticker.dimensions = Some((64, 64));
        assert_eq!(store.slot(&sticker, 80), Slot::Cells(7, 4));
        // One that does not say its size: the box of a sticker, not of a photo.
        sticker.dimensions = None;
        assert_eq!(store.slot(&sticker, 80), Slot::Cells(18, 7));
        // A narrow pane bounds it like every picture.
        assert_eq!(store.slot(&sticker, 10), Slot::Cells(10, 3));
        // One that claims an absurd shape still takes at least one cell and at most
        // the box.
        sticker.dimensions = Some((100_000, 1));
        assert_eq!(store.slot(&sticker, 80), Slot::Cells(18, 1));
        sticker.dimensions = Some((1, 100_000));
        assert_eq!(store.slot(&sticker, 80), Slot::Cells(1, 7));
    }

    /// The checksum PNG uses for its chunks.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
            }
        }
        !crc
    }

    #[test]
    fn terminal_answers_decide_how_pictures_are_drawn() {
        use crate::termquery::Reply;
        let kitty = Reply {
            kitty: true,
            compression: true,
            cell: Some((9, 18)),
            complete: true,
            ..Reply::default()
        };
        assert_eq!(
            Graphics::from_reply(kitty, Some((7, 14))),
            Graphics {
                protocol: Some(Protocol::Kitty { compress: true }),
                font: (9, 18)
            }
        );
        // The cell size may come from the system instead.
        let no_cell = Reply {
            cell: None,
            ..kitty
        };
        assert_eq!(Graphics::from_reply(no_cell, Some((7, 14))).font, (7, 14));
        // Without any size a picture cannot be laid out; without kitty or without an
        // answer (the timeout) it is half blocks.
        assert_eq!(Graphics::from_reply(no_cell, None), Graphics::halfblocks());
        let plain = Reply {
            kitty: false,
            ..kitty
        };
        assert_eq!(Graphics::from_reply(plain, None), Graphics::halfblocks());
        assert_eq!(
            Graphics::from_reply(Reply::default(), Some((7, 14))),
            Graphics::halfblocks()
        );
    }

    #[test]
    fn room_pictures_are_cut_to_fill_their_cells() {
        // The part of the picture with the shape of the cells, as large as possible.
        assert_eq!(cover_crop((96, 96), (20, 20)), (96, 96));
        assert_eq!(cover_crop((96, 96), (16, 20)), (76, 96));
        assert_eq!(cover_crop((96, 96), (24, 20)), (96, 80));
        assert_eq!(cover_crop((200, 50), (20, 20)), (50, 50));
        assert_eq!(cover_crop((0, 0), (0, 0)), (1, 1));

        // Any shape of file becomes a square of the kept size, cut from the middle.
        let mut wide = Vec::new();
        demo_picture(400, 100)
            .write_to(&mut Cursor::new(&mut wide), image::ImageFormat::Png)
            .unwrap();
        let avatar = decode_avatar(&wide).unwrap();
        assert_eq!((avatar.pixels.width(), avatar.pixels.height()), (96, 96));
        assert_eq!(avatar.original, (400, 100));
        // A small one is not blown up.
        let mut small = Vec::new();
        demo_picture(30, 40)
            .write_to(&mut Cursor::new(&mut small), image::ImageFormat::Png)
            .unwrap();
        let avatar = decode_avatar(&small).unwrap();
        assert_eq!((avatar.pixels.width(), avatar.pixels.height()), (30, 30));
        assert!(decode_avatar(b"junk").is_err());

        // Encoded for two cells at this font: 2x10 by 20 pixels.
        let encoded = encode_avatar(&avatar.pixels, kitty(), 7, (2, 1)).unwrap();
        assert_eq!(encoded.size, (2, 1));
        assert!(encode_avatar(&avatar.pixels, Graphics::halfblocks(), 7, (2, 1)).is_none());
    }

    #[test]
    fn room_pictures_have_their_own_limit() {
        let mut store = ImageStore::new(kitty());
        for n in 0..AVATAR_ENTRIES + 50 {
            store.begin_frame();
            let url = format!("mxc://x/{n}");
            assert!(store.avatar(&url, (2, 1), false).is_none());
            store.on_avatar(&url, Ok(decoded(96, 96)));
            assert!(store.avatar(&url, (2, 1), false).is_some());
        }
        assert_eq!(store.avatars.len(), AVATAR_ENTRIES);
        // The oldest are gone, their copies in the terminal are freed, and the
        // pictures in messages are not touched by any of it.
        assert!(!store.avatars.contains_key("mxc://x/0"));
        assert_eq!(store.cleanup_sequence(false).matches("a=d,d=I").count(), 50);
        assert!(store.entries.is_empty());
        assert_eq!(store.take_avatar_requests().len(), AVATAR_ENTRIES + 50);
    }

    #[test]
    fn a_room_picture_is_made_for_each_size_it_is_drawn_at_and_kept() {
        let mut store = ImageStore::new(kitty());
        let url = "mxc://x/face";
        let delete = |id: Option<u32>| format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", id.unwrap());
        store.begin_frame();
        assert!(store.avatar(url, (2, 1), false).is_none());
        store.on_avatar(url, Ok(decoded(96, 96)));

        // Two cells by one in most places, four by two in the room list with room
        // previews: each has its own, made for its cells.
        let small = store.avatar(url, (2, 1), false).unwrap();
        assert_eq!(small.size, (2, 1));
        let small = small.kitty_id;
        let large = store.avatar(url, (4, 2), false).unwrap();
        assert_eq!(large.size, (4, 2));
        let large = large.kitty_id;
        assert_ne!(small, large);

        // Drawn at the one and the other in turn, as a narrow window going between
        // the room list and the chat does: neither is made again.
        for _ in 0..3 {
            store.begin_frame();
            assert_eq!(store.avatar(url, (4, 2), false).unwrap().kitty_id, large);
            store.begin_frame();
            assert_eq!(store.avatar(url, (2, 1), false).unwrap().kitty_id, small);
        }
        assert!(store.cleanup_sequence(false).is_empty());

        // A transmission that was painted over costs only the size it was for.
        store.sent_avatar(url, (4, 2), 3, 4);
        store.verify(|_, _| false);
        assert_eq!(store.cleanup_sequence(false), delete(large));
        assert_eq!(store.avatar(url, (2, 1), false).unwrap().kitty_id, small);
        let large = store.avatar(url, (4, 2), false).unwrap().kitty_id;

        // Three sizes are kept; a fourth pushes out the one drawn at longest ago.
        store.begin_frame();
        store.avatar(url, (3, 1), false).unwrap();
        store.avatar(url, (2, 1), false).unwrap();
        store.avatar(url, (3, 1), false).unwrap();
        assert!(store.cleanup_sequence(false).is_empty());
        store.avatar(url, (6, 3), false).unwrap();
        assert_eq!(store.cleanup_sequence(false), delete(large));
        assert_eq!(store.avatar(url, (2, 1), false).unwrap().kitty_id, small);
    }

    #[test]
    fn a_room_picture_on_the_screen_is_not_pushed_out_by_another_size() {
        let mut store = ImageStore::new(kitty());
        let url = "mxc://x/face";
        let delete = |id: Option<u32>| format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", id.unwrap());
        store.begin_frame();
        assert!(store.avatar(url, (2, 1), false).is_none());
        store.on_avatar(url, Ok(decoded(96, 96)));

        // Four sizes in one frame, one more than are kept: all four are on the
        // screen, so none is deleted, and each keeps the one made for it.
        store.begin_frame();
        let sizes = [(2, 1), (4, 2), (3, 1), (6, 3)];
        let ids: Vec<_> = sizes
            .iter()
            .map(|&cells| store.avatar(url, cells, false).unwrap().kitty_id)
            .collect();
        for (&cells, &id) in sizes.iter().zip(&ids) {
            assert_eq!(store.avatar(url, cells, false).unwrap().kitty_id, id);
        }
        assert!(store.cleanup_sequence(false).is_empty());

        // A frame that draws fewer lets the one drawn at longest ago go.
        store.begin_frame();
        store.avatar(url, (6, 3), false).unwrap();
        assert_eq!(store.cleanup_sequence(false), delete(ids[0]));
        for (&cells, &id) in sizes.iter().zip(&ids).skip(1) {
            assert_eq!(store.avatar(url, cells, false).unwrap().kitty_id, id);
        }
        assert!(store.cleanup_sequence(false).is_empty());
    }

    #[test]
    fn a_size_of_a_room_picture_that_cannot_be_made_pushes_out_none() {
        let mut store = ImageStore::new(kitty());
        let url = "mxc://x/face";
        store.begin_frame();
        assert!(store.avatar(url, (2, 1), false).is_none());
        store.on_avatar(url, Ok(decoded(96, 96)));
        let sizes = [(2, 1), (4, 2), (3, 1)];
        let ids: Vec<_> = sizes
            .iter()
            .map(|&cells| store.avatar(url, cells, false).unwrap().kitty_id)
            .collect();

        // As many sizes as are kept, and in a later frame one that is not made (no
        // cells at all): none of the three is deleted for it.
        store.begin_frame();
        assert!(store.avatar(url, (0, 1), false).is_none());
        assert!(store.cleanup_sequence(false).is_empty());
        for (&cells, &id) in sizes.iter().zip(&ids) {
            assert_eq!(store.avatar(url, cells, false).unwrap().kitty_id, id);
        }
        assert!(store.cleanup_sequence(false).is_empty());
    }

    #[test]
    fn remembered_sizes_are_bounded() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let kept = remote_image("mxc://x/kept", None);
        store.begin_frame();
        store.request(&kept);
        store.on_loaded(&kept.key, false, Ok(decoded(20, 10)));
        for n in 0..LEARNED_ENTRIES + 10 {
            store.learn(&format!("mxc://x/{n}"), (1, 1));
        }
        assert!(store.learned.len() <= LEARNED_ENTRIES);
        // What is still in the cache keeps its size.
        assert_eq!(store.learned.get(&kept.key), Some(&(20, 10)));
    }

    #[test]
    fn a_dragged_window_starts_one_viewer_encoding_at_a_time() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        store.set_encoder(sender);
        let image = remote_image("mxc://x/a", Some((2000, 1000)));
        store.open_viewer(&image);
        store.on_loaded(&image.key, true, Ok(decoded(400, 200)));
        // Many sizes in a row while the first encoding is still running.
        for cols in 20..40 {
            assert!(matches!(
                store.viewer_content((cols, 10), false),
                ViewerContent::Preparing
            ));
        }
        let first = receiver.blocking_recv().unwrap();
        assert_eq!(first.size, (20, 10));
        // Nothing else was started.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(receiver.try_recv().is_err());
        store.on_encoded(first);
        // The window ended up at another size: now that one is made.
        assert!(matches!(
            store.viewer_content((39, 10), false),
            ViewerContent::Preparing
        ));
        let second = receiver.blocking_recv().unwrap();
        assert_eq!(second.size, (39, 10));
        store.on_encoded(second);
        assert!(matches!(
            store.viewer_content((39, 10), false),
            ViewerContent::Ready(_)
        ));

        // One that was under way when the terminal forgot its pictures is not used.
        assert!(matches!(
            store.viewer_content((30, 10), false),
            ViewerContent::Preparing
        ));
        store.forget_encoded();
        store.on_encoded(receiver.blocking_recv().unwrap());
        assert!(matches!(
            store.viewer_content((30, 10), false),
            ViewerContent::Preparing
        ));
        store.on_encoded(receiver.blocking_recv().unwrap());
        assert!(matches!(
            store.viewer_content((30, 10), false),
            ViewerContent::Ready(_)
        ));
    }

    #[test]
    fn layout_follows_what_is_known_about_a_picture() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let sized = remote_image("mxc://x/sized", Some((800, 600)));
        let unsized_ = remote_image("mxc://x/unsized", None);
        assert_eq!(store.slot(&sized, 80), Slot::Cells(32, 12));
        assert_eq!(store.slot(&unsized_, 80), Slot::Cells(24, 8));
        assert_eq!(store.slot(&unsized_, 10), Slot::Cells(10, 3));

        // The size is learned when the picture arrives, and stays known.
        let before = store.layout_rev;
        store.begin_frame();
        store.request(&unsized_);
        store.on_loaded(&unsized_.key, false, Ok(decoded(200, 100)));
        assert!(store.layout_rev > before);
        assert_eq!(store.slot(&unsized_, 80), Slot::Cells(20, 5));

        // A failure takes one line.
        store.request(&sized);
        store.on_loaded(&sized.key, false, Err("too large (34 MB)".into()));
        assert_eq!(
            store.slot(&sized, 80),
            Slot::Failed("too large (34 MB)".into())
        );
        assert_eq!(store.failure(&sized.key), Some("too large (34 MB)"));

        // Off: no space is kept at all.
        let off = ImageStore::new(Graphics::off());
        assert_eq!(off.slot(&sized, 80), Slot::Off);
    }

    #[test]
    fn requests_are_made_once_and_results_nobody_waits_for_are_dropped() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let image = remote_image("mxc://x/a", Some((100, 100)));
        store.begin_frame();
        store.request(&image);
        store.request(&image);
        store.begin_frame();
        store.request(&image);
        let requests = store.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].full);
        assert!(store.is_loading(&image.key));
        assert!(store.inline(&image.key, (10, 5), false).is_none());

        store.on_loaded(&image.key, false, Ok(decoded(100, 100)));
        assert!(!store.is_loading(&image.key));
        assert!(store.inline(&image.key, (10, 5), false).is_some());
        // Still there: no second request.
        store.request(&image);
        assert!(store.take_requests().is_empty());

        // A result for something never asked for is ignored.
        store.on_loaded("mxc://x/other", false, Ok(decoded(10, 10)));
        assert!(store.inline("mxc://x/other", (10, 5), false).is_none());

        // With pictures off nothing is requested.
        let mut off = ImageStore::new(Graphics::off());
        off.request(&image);
        assert!(off.take_requests().is_empty());
    }

    #[test]
    fn the_cache_drops_what_was_unused_for_longest() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        // 640x480 RGBA is about 1.2 MB, so 64 MB hold a bit over fifty of them.
        let images: Vec<ImageInfo> = (0..80)
            .map(|n| remote_image(&format!("mxc://x/{n}"), Some((640, 480))))
            .collect();
        for image in &images {
            store.begin_frame();
            store.request(image);
            store.on_loaded(&image.key, false, Ok(decoded(640, 480)));
        }
        let bytes: usize = store.entries.values().map(Entry::bytes).sum();
        assert!(bytes <= CACHE_BYTES, "{bytes}");
        assert!(store.entries.len() < images.len());
        // The newest are kept, the oldest are gone and would be asked for again.
        assert!(store.inline(&images[79].key, (10, 5), false).is_some());
        assert!(!store.entries.contains_key(&images[0].key));
        store.take_requests();
        store.begin_frame();
        store.request(&images[0]);
        assert_eq!(store.take_requests().len(), 1);
        // Its size is still known, so the layout does not change while it reloads.
        assert_eq!(store.learned.get(&images[0].key), Some(&(640, 480)));
    }

    #[test]
    fn kitty_pictures_are_deleted_from_the_terminal_when_dropped() {
        let mut store = ImageStore::new(kitty());
        let image = remote_image("mxc://x/a", Some((100, 100)));
        store.begin_frame();
        store.request(&image);
        store.on_loaded(&image.key, false, Ok(decoded(100, 100)));
        assert!(store.cleanup_sequence(false).is_empty());

        let first = store.inline(&image.key, (10, 5), false).unwrap().kitty_id;
        assert!(first.is_some());
        // Another size: the old encoding is of no use any more.
        let second = store.inline(&image.key, (8, 4), false).unwrap().kitty_id;
        assert_ne!(first, second);
        let sequence = store.cleanup_sequence(false);
        assert_eq!(
            sequence,
            format!("\x1b_Ga=d,d=I,i={},q=2\x1b\\", first.unwrap())
        );
        // Quitting deletes the rest.
        let sequence = store.cleanup_sequence(true);
        assert!(sequence.contains(&format!("i={},", second.unwrap())));
        assert!(store.cleanup_sequence(true).is_empty());

        // Half blocks leave nothing in the terminal.
        let mut plain = ImageStore::new(Graphics::halfblocks());
        plain.begin_frame();
        plain.request(&image);
        plain.on_loaded(&image.key, false, Ok(decoded(100, 100)));
        plain.inline(&image.key, (10, 5), false).unwrap();
        assert!(plain.cleanup_sequence(true).is_empty());
    }

    #[test]
    fn a_transmission_that_was_painted_over_is_made_again() {
        let mut store = ImageStore::new(kitty());
        let image = remote_image("mxc://x/a", Some((100, 100)));
        store.begin_frame();
        store.request(&image);
        store.on_loaded(&image.key, false, Ok(decoded(100, 100)));

        // While something may cover it, nothing new is transmitted.
        assert!(store.inline(&image.key, (10, 5), true).is_none());

        let encoded = store.inline(&image.key, (10, 5), false).unwrap();
        assert!(!encoded.shown);
        let id = encoded.kitty_id;
        store.sent_inline(&image.key, 3, 4);
        // The cell was overwritten: the encoding is thrown away and redone.
        store.verify(|_, _| false);
        let encoded = store.inline(&image.key, (10, 5), false).unwrap();
        assert_ne!(encoded.kitty_id, id);
        store.sent_inline(&image.key, 3, 4);
        store.verify(|x, y| (x, y) == (3, 4));
        // Now it is with the terminal, and a popup on top is no problem any more.
        let encoded = store.inline(&image.key, (10, 5), true).unwrap();
        assert!(encoded.shown);
    }

    #[test]
    fn viewer_loads_the_whole_file_and_rescues_a_failed_thumbnail() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let image = remote_image("mxc://x/a", Some((2000, 1000)));
        store.begin_frame();
        store.request(&image);
        store.on_loaded(&image.key, false, Err("could not be loaded".into()));
        store.take_requests();

        store.open_viewer(&image);
        let requests = store.take_requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].full);
        assert!(matches!(
            store.viewer_content((80, 20), false),
            ViewerContent::Loading
        ));
        // A result for another picture changes nothing.
        store.on_loaded("mxc://x/other", true, Ok(decoded(10, 10)));
        assert!(matches!(
            store.viewer_content((80, 20), false),
            ViewerContent::Loading
        ));

        store.on_loaded(&image.key, true, Ok(decoded(2000, 1000)));
        assert_eq!(store.viewer_pixels(), Some((2000, 1000)));
        match store.viewer_content((80, 20), false) {
            ViewerContent::Ready(encoded) => assert_eq!(encoded.size, (80, 20)),
            _ => panic!("not ready"),
        }
        // The message's picture works again too.
        assert_eq!(store.failure(&image.key), None);
        assert!(store.inline(&image.key, (40, 10), false).is_some());

        store.close_viewer();
        assert!(!store.viewer_open());
        // A late result for the closed viewer is ignored.
        store.on_loaded(&image.key, true, Err("late".into()));
        assert!(!store.viewer_open());
    }

    #[test]
    fn viewer_failure_is_reported() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let image = remote_image("mxc://x/a", None);
        store.open_viewer(&image);
        store.on_loaded(&image.key, true, Err("too large (34 MB)".into()));
        assert!(matches!(
            store.viewer_content((80, 20), false),
            ViewerContent::Failed("too large (34 MB)")
        ));
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1500), "1.5 kB");
        assert_eq!(human_size(34_000_000), "34 MB");
        assert_eq!(human_size(1_200_000), "1.2 MB");
    }

    #[test]
    fn demo_picture_has_colour() {
        let picture = demo_picture(80, 40).into_rgba8();
        let left = picture.get_pixel(2, 2);
        let right = picture.get_pixel(77, 2);
        let bottom = picture.get_pixel(2, 38);
        assert_ne!(left, right);
        assert_ne!(left, bottom);
        // Warm on the left, cool on the right.
        assert!(left[0] > left[2]);
        assert!(right[2] > right[0]);
    }

    #[test]
    fn custom_emoji_pictures_are_only_fetched_for_kitty_and_a_few_at_a_time() {
        // Half blocks or pictures off: the names stay, nothing is fetched.
        for graphics in [Graphics::halfblocks(), Graphics::off()] {
            let mut store = ImageStore::new(graphics);
            store.begin_frame();
            store.request_emote("mxc://example.org/blob");
            assert!(store.take_emote_requests().is_empty());
            assert!(!store.emote_pictures());
            assert!(!store.emote_ready("mxc://example.org/blob"));
        }

        let mut store = ImageStore::new(kitty());
        store.begin_frame();
        // Only plain `mxc://` addresses are asked for.
        for bad in ["https://example.org/blob", "mxc://example.org/a/b", ""] {
            store.request_emote(bad);
        }
        assert!(store.take_emote_requests().is_empty());

        // A screen full of new ones: a few are on their way, the rest wait.
        let urls: Vec<String> = (0..20).map(|n| format!("mxc://example.org/e{n}")).collect();
        for url in &urls {
            store.request_emote(url);
        }
        let first = store.take_emote_requests();
        assert_eq!(first.len(), EMOTE_IN_FLIGHT);
        // Asked again on the next frame: nothing more while those are on their way.
        store.begin_frame();
        for url in &urls {
            store.request_emote(url);
        }
        assert!(store.take_emote_requests().is_empty());
        // One answer makes room for one more.
        store.on_emote(&first[0], Ok(decoded(64, 64)));
        store.begin_frame();
        for url in &urls {
            store.request_emote(url);
        }
        assert_eq!(store.take_emote_requests(), [urls[EMOTE_IN_FLIGHT].clone()]);
    }

    #[test]
    fn a_custom_emoji_picture_that_arrives_changes_the_layout_and_one_that_fails_does_not() {
        let mut store = ImageStore::new(kitty());
        let (good, bad) = ("mxc://example.org/good", "mxc://example.org/bad");
        store.begin_frame();
        store.request_emote(good);
        store.request_emote(bad);
        assert_eq!(store.take_emote_requests().len(), 2);
        assert!(!store.emote_ready(good));
        assert!(store.emote(good, false).is_none());

        let before = store.emote_rev;
        store.on_emote(bad, Err("could not be loaded".into()));
        assert_eq!(store.emote_rev, before);
        assert!(!store.emote_ready(bad));
        // A failure is not asked for again.
        store.request_emote(bad);
        assert!(store.take_emote_requests().is_empty());

        store.on_emote(good, Ok(decoded(128, 64)));
        assert!(store.emote_rev > before);
        assert!(store.emote_ready(good));
        // Nothing new is sent to the terminal while a popup may cover it.
        assert!(store.emote(good, true).is_none());
        let encoded = store.emote(good, false).unwrap();
        assert_eq!(encoded.size, EMOTE_CELLS);
        // The whole box, 2x10 by 20 pixels, transparent where the picture is not.
        assert_eq!(encoded.bytes, 20 * 20 * 4);
        assert!(!encoded.shown);
        store.sent_emote(good, 4, 2);
        store.verify(|x, y| (x, y) == (4, 2));
        assert!(store.emote(good, true).unwrap().shown);

        // A late or unasked-for answer changes nothing.
        let rev = store.emote_rev;
        store.on_emote(good, Ok(decoded(10, 10)));
        store.on_emote("mxc://example.org/never", Ok(decoded(10, 10)));
        assert_eq!(store.emote_rev, rev);
        assert!(!store.emote_ready("mxc://example.org/never"));

        // Logging out forgets them, and the lines show names again.
        store.clear();
        assert!(!store.emote_ready(good));
        assert!(store.emote_rev > rev);
    }

    #[test]
    fn a_custom_emoji_whose_data_is_painted_over_goes_with_another_copy_in_the_next_frame() {
        let mut store = ImageStore::new(kitty());
        let url = "mxc://example.org/blob";
        store.begin_frame();
        store.request_emote(url);
        store.on_emote(url, Ok(decoded(64, 64)));

        // The copy at (2, 3) carries the data, and something drawn later in the frame
        // goes over it: the picture is freed, and the frame is to be drawn again.
        assert!(!store.emote_painted_over(2, 3));
        assert!(store.emote(url, false).is_some());
        store.sent_emote(url, 2, 3);
        store.verify(|_, _| false);
        assert!(store.take_redraw());
        assert!(!store.take_redraw());
        assert_eq!(store.cleanup_sequence(false).matches("a=d,d=I").count(), 1);

        // Then that cell is passed over, and the copy at (10, 5) carries it.
        store.begin_frame();
        assert!(store.emote_painted_over(2, 3));
        assert!(store.emote(url, true).is_none());
        assert!(!store.emote(url, false).unwrap().shown);
        store.sent_emote(url, 10, 5);
        store.verify(|x, y| (x, y) == (10, 5));
        assert!(!store.take_redraw());
        assert!(!store.emote_painted_over(2, 3));

        // From then on every copy refers to it, wherever it is: nothing is sent or
        // freed again, and no frame asks for another.
        for _ in 0..3 {
            store.begin_frame();
            assert!(store.emote(url, false).unwrap().shown);
            store.verify(|_, _| false);
            assert!(!store.take_redraw());
            assert!(store.cleanup_sequence(false).is_empty());
        }
    }

    #[test]
    fn custom_emoji_pictures_have_their_own_limit() {
        let mut store = ImageStore::new(kitty());
        for n in 0..EMOTE_ENTRIES + 20 {
            store.begin_frame();
            let url = format!("mxc://x/e{n}");
            store.request_emote(&url);
            store.on_emote(&url, Ok(decoded(64, 64)));
            assert!(store.emote(&url, false).is_some());
        }
        assert_eq!(store.emotes.len(), EMOTE_ENTRIES);
        // The oldest are gone, their copies in the terminal are freed, and room
        // pictures and pictures in messages are not touched by any of it.
        assert!(!store.emote_ready("mxc://x/e0"));
        assert!(store.emote_ready(&format!("mxc://x/e{}", EMOTE_ENTRIES + 19)));
        assert_eq!(store.cleanup_sequence(false).matches("a=d,d=I").count(), 20);
        assert!(store.entries.is_empty() && store.avatars.is_empty());
    }

    #[test]
    fn request_avatar_fetches_pixels_even_without_kitty() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let url = "mxc://example.org/room-avatar";
        // First request queues a fetch and returns None immediately.
        assert!(store.request_avatar(url).is_none());
        assert_eq!(store.take_avatar_requests(), vec![url.to_owned()]);
        // A duplicate request while Loading does not enqueue a second fetch.
        assert!(store.request_avatar(url).is_none());
        assert!(store.take_avatar_requests().is_empty());
        // Once delivered, request_avatar returns the 96x96 pixels.
        store.on_avatar(url, Ok(decoded(96, 96)));
        assert_eq!(store.request_avatar(url).unwrap().width(), 96);
        assert_eq!(store.request_avatar(url).unwrap().height(), 96);
    }
}
