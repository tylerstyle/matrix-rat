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
/// Space kept for a picture whose size is not known yet.
const UNKNOWN_CELLS: (u16, u16) = (24, 8);
/// Pictures between messages are kept no larger than this, in pixels.
pub const THUMBNAIL_PIXELS: (u32, u32) = (640, 480);
/// The viewer never needs more than a large screen's worth.
pub const FULL_PIXELS: (u32, u32) = (3840, 2160);
/// Files above this are not downloaded.
pub const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
/// Decoding refuses pictures wider or taller than this (decompression bombs).
const MAX_DECODE_SIDE: u32 = 16_384;
const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;
/// Memory the cache may hold before the pictures unused for longest are dropped.
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const CACHE_ENTRIES: usize = 256;
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
    /// run). Takes at most [`QUERY_TIMEOUT`]; without an answer it is half blocks.
    ///
    /// Only kitty's protocol with Unicode placeholders is used for real graphics: its
    /// pictures live in ordinary cells, so they cannot be left behind or painted over
    /// popups. Terminals that only do sixel or the iTerm2 protocol get half blocks, and
    /// so does everything inside a multiplexer.
    pub fn detect(mode: ImageMode) -> Self {
        match mode {
            ImageMode::Off => return Self::off(),
            ImageMode::Halfblocks => return Self::halfblocks(),
            ImageMode::Auto => {}
        }
        if in_multiplexer() {
            tracing::info!("images: inside a terminal multiplexer, using half blocks");
            return Self::halfblocks();
        }
        if placeholders_broken() {
            tracing::info!(
                "images: this terminal's kitty placeholders are unreliable, using half blocks"
            );
            return Self::halfblocks();
        }
        let Some(reply) = termquery::ask(QUERY_TIMEOUT) else {
            return Self::halfblocks();
        };
        let graphics = Self::from_reply(reply, termquery::cell_from_window());
        tracing::info!(?reply, ?graphics, "images: terminal queried");
        graphics
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
fn placeholders_broken() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    set("WEZTERM_EXECUTABLE") || set("KONSOLE_VERSION")
}

fn in_multiplexer() -> bool {
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
    let reason = |err: image::ImageError| match err {
        image::ImageError::Limits(_) => "too large to show".to_owned(),
        image::ImageError::Unsupported(_) => "not a picture this app can read".to_owned(),
        _ => "the file is damaged".to_owned(),
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
            })
        }
    }
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
            _ => 0,
        }
    }
}

/// How a picture message is laid out in the timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    /// Pictures are off: the message shows its text instead.
    Off,
    /// One line saying why there is no picture.
    Failed(String),
    /// Columns and rows kept free for the picture.
    Cells(u16, u16),
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

    fn discard(garbage: &mut Vec<u32>, encoded: Option<Encoded>) {
        // Only what the terminal may have received needs deleting there, but an id it
        // never saw does no harm either.
        if let Some(id) = encoded.and_then(|encoded| encoded.kitty_id) {
            garbage.push(id);
        }
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
        let limit = inline_limit(width);
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
                _ => return,
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
        self.layout_rev += 1;
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
            _ => return None,
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
            _ => None,
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
    /// over the timeline later in a frame either sets `covered` (popups, the viewer),
    /// which holds new transmissions back, or is caught here.
    pub fn verify(&mut self, intact: impl Fn(u16, u16) -> bool) {
        for (target, x, y) in std::mem::take(&mut self.unverified) {
            let slot = match &target {
                Target::Inline(key) => match self.entries.get_mut(key) {
                    Some(Entry {
                        state: State::Ready { encoded, .. },
                        ..
                    }) => encoded,
                    _ => continue,
                },
                Target::Viewer => match self.viewer.as_mut() {
                    Some(viewer) => &mut viewer.encoded,
                    None => continue,
                },
            };
            if intact(x, y) {
                if let Some(encoded) = slot {
                    encoded.shown = true;
                }
            } else {
                Self::discard(&mut self.garbage, slot.take());
            }
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
            _ => None,
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
}
