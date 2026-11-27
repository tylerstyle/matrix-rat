//! The cache of pictures: decoded pixels, and what the terminal is sent for them at
//! each size (ratatui-image's kitty and half-block encodings); the viewer's encodings
//! are made on a thread of their own.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use image::DynamicImage;
#[cfg(not(target_family = "wasm"))]
use image::RgbaImage;
#[cfg(not(target_family = "wasm"))]
use image::imageops::FilterType;
#[cfg(not(target_family = "wasm"))]
use ratatui::layout::Size;
#[cfg(not(target_family = "wasm"))]
use ratatui_image::protocol::halfblocks::Halfblocks;
#[cfg(not(target_family = "wasm"))]
use ratatui_image::protocol::kitty::Kitty;
#[cfg(not(target_family = "wasm"))]
use ratatui_image::sliced::SlicedProtocol;
#[cfg(not(target_family = "wasm"))]
use ratatui_image::{FontSize, Resize};
#[cfg(any(not(target_family = "wasm"), test))]
use tokio::sync::mpsc::UnboundedSender;

use super::{
    Decoded, EMOTE_CELLS, EMOTE_IN_FLIGHT, Graphics, Pixels, Protocol, Request, STICKER_MAX_CELLS,
    Slot, THUMBNAIL_PIXELS, UNKNOWN_CELLS, demo_picture, fit_cells, inline_limit,
};
use crate::model::{DEMO_IMAGE, ImageInfo, ImageSource};
use crate::time::Instant;

/// Memory the cache may hold before the pictures unused for longest are dropped.
const CACHE_BYTES: usize = 64 * 1024 * 1024;
const CACHE_ENTRIES: usize = 256;
/// Room pictures kept; at 96x96 each that is some 20 MB at the very most. On top of
/// that each keeps, for each size it is drawn at, what the terminal is sent: kitty's
/// base64 of the pixels of those cells, which is a third more than their four bytes
/// each. That is at most [`AVATAR_SIZES`] for each, so 512 x 3 in all; with cells of
/// 10 by 20 pixels, some 2 KB for two cells and 9 KB for four by two, some 6 MB for
/// all 512 at both. It grows with the pixels of a cell.
const AVATAR_ENTRIES: usize = 512;
/// Sizes a room picture is kept made for at once: it is drawn at two (see
/// [`ImageStore::avatar`]), and one more is spare. Frames that draw it at more sizes
/// keep them all, until two frames in a row draw fewer.
const AVATAR_SIZES: usize = 3;
/// Custom emoji pictures kept; at 64x64 each that is some 4 MB at the very most.
const EMOTE_ENTRIES: usize = 256;
/// Sizes remembered for pictures whose message did not state one.
const LEARNED_ENTRIES: usize = 4096;
/// A request the worker never answered (it restarted) may be made again after this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// A custom emoji picture that failed is fetched again, when it is drawn again, after
/// this long; each failure in a row doubles it, up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(60);
const RETRY_MAX: Duration = Duration::from_secs(30 * 60);
/// Above this many pixels a kitty transmission is compressed, if the terminal can.
#[cfg(any(not(target_family = "wasm"), test))]
const COMPRESS_ABOVE: u32 = 512 * 512;
/// Why the viewer shows no picture where nothing draws one (shown after its name).
#[cfg(any(target_family = "wasm", test))]
const NOT_DRAWN: &str = "pictures are not drawn in the browser yet";

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What the terminal is sent for a picture. The browser draws no picture into cells
/// yet (phase 4 does): there it has no value, so no [`Encoded`] is ever made, and every
/// picture keeps its placeholder.
#[cfg(target_family = "wasm")]
pub enum SlicedProtocol {}

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
    /// drawn at; one drawn in the current frame or the one before stays.
    drawn: u64,
}

/// Turns pixels into what the terminal draws. `None` if pictures are off or the
/// encoding failed.
#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
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
#[cfg(any(not(target_family = "wasm"), test))]
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
#[cfg(not(target_family = "wasm"))]
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

/// The browser encodes nothing (see [`SlicedProtocol`]).
#[cfg(target_family = "wasm")]
fn encode(
    _pixels: &DynamicImage,
    _size: (u16, u16),
    _graphics: Graphics,
    _kitty_id: u32,
) -> Option<Encoded> {
    None
}

#[cfg(target_family = "wasm")]
fn encode_avatar(
    _pixels: &DynamicImage,
    _graphics: Graphics,
    _kitty_id: u32,
    _cells: (u16, u16),
) -> Option<Encoded> {
    None
}

#[cfg(target_family = "wasm")]
fn encode_emote(
    _pixels: &DynamicImage,
    _graphics: Graphics,
    _kitty_id: u32,
    _cells: (u16, u16),
) -> Option<Encoded> {
    None
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

/// A viewer encoding finished on another thread. The terminal's alone: the browser
/// has no thread to encode on (`App::start_images` is the terminal's).
#[cfg(any(not(target_family = "wasm"), test))]
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
    /// How many fetches failed in a row.
    failures: u32,
}

impl Small {
    /// The fetch failed: the picture is not asked for again before [`retry_wait`].
    fn fail(&mut self) {
        self.failures = self.failures.saturating_add(1);
        self.state = SmallState::Failed {
            retry_at: Instant::now() + retry_wait(self.failures),
        };
    }

    /// Asked for, and not given up on: the worker may still answer.
    fn on_its_way(&self) -> bool {
        matches!(self.state, SmallState::Loading { since } if since.elapsed() < REQUEST_TIMEOUT)
    }
}

/// The pause after the `failures`th failure in a row: [`RETRY_FIRST`], doubling.
fn retry_wait(failures: u32) -> Duration {
    RETRY_FIRST
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(RETRY_MAX)
}

enum SmallState {
    Loading {
        since: Instant,
    },
    /// The list shows the coloured tile instead (a custom emoji its name), without a
    /// word. May be fetched again from `retry_at` on (custom emoji only).
    Failed {
        retry_at: Instant,
    },
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
    #[cfg(any(not(target_family = "wasm"), test))]
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
        // Ids start somewhere arbitrary so two instances in one terminal do not clash: the
        // standard library's randomly keyed hasher gives the start (the keys come from the
        // system's random source, so two processes differ however close in time they start),
        // and a counter keeps stores of one process apart should the hasher repeat itself.
        static STORES: AtomicU32 = AtomicU32::new(0);
        let seed = std::hash::RandomState::new()
            .hash_one(STORES.fetch_add(1, Ordering::Relaxed))
            .to_le_bytes();
        let seed = u32::from_le_bytes([seed[0], seed[1], seed[2], seed[3]])
            ^ u32::from_le_bytes([seed[4], seed[5], seed[6], seed[7]]);
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
            #[cfg(any(not(target_family = "wasm"), test))]
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
    #[cfg(any(not(target_family = "wasm"), test))]
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
                    failures: 0,
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
            SmallState::Loading { .. } | SmallState::Failed { .. } => None,
        }
    }

    /// The picture of a room or a person ready to draw into `cells` (columns and rows),
    /// if there is one yet. Asks for it otherwise. Only with real graphics: two cells of
    /// half blocks are four pixels. `covered` as for [`ImageStore::inline`].
    ///
    /// It is made for each size it is drawn at, and kept for each: the room list draws
    /// it four cells by two with room previews on, everything else two by one, and the
    /// two are on screen together or take turns in a narrow window.
    #[cfg_attr(
        target_family = "wasm",
        expect(
            unreachable_code,
            reason = "the browser's Encoded is never made (SlicedProtocol), so nothing gets past taking one out"
        )
    )]
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
        // this frame: that one is on the screen, and deleting it would blank it. Nor
        // one drawn in the frame before, which this frame may not have come to yet:
        // a picture drawn at more sizes every frame would otherwise make, send and
        // delete them again each time. As they are in the order drawn, once the first
        // must stay all must; then the picture keeps more, until two frames in a row
        // draw fewer of them.
        while encoded.len() > AVATAR_SIZES && encoded[0].drawn + 1 < frame {
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
        match result {
            Ok(decoded) => {
                avatar.failures = 0;
                avatar.state = SmallState::Ready {
                    pixels: decoded.pixels,
                    encoded: Vec::new(),
                };
            }
            Err(_) => avatar.fail(),
        }
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
                // One that failed is tried again once its pause is over, and only as
                // it is drawn again.
                if let SmallState::Failed { retry_at } = small.state
                    && Instant::now() >= retry_at
                    && loading < EMOTE_IN_FLIGHT
                {
                    small.state = SmallState::Loading {
                        since: Instant::now(),
                    };
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
                    failures: 0,
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
        match result {
            Ok(decoded) => {
                // Two cells instead of the name: the lines are laid out again.
                self.emote_rev += 1;
                small.failures = 0;
                small.state = SmallState::Ready {
                    pixels: decoded.pixels,
                    encoded: Vec::new(),
                };
            }
            // The name stays where it is, and the picture is tried again later.
            Err(_) => small.fail(),
        }
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
    #[cfg_attr(
        target_family = "wasm",
        expect(
            unreachable_code,
            reason = "the browser's Encoded is never made (SlicedProtocol), so nothing gets past taking one out"
        )
    )]
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
    #[cfg(any(not(target_family = "wasm"), test))]
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
        #[cfg(target_family = "wasm")]
        self.nothing_draws_the_viewer();
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
        #[cfg(any(not(target_family = "wasm"), test))]
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
            #[cfg(any(not(target_family = "wasm"), test))]
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
            // No thread to encode on in the browser (where nothing gets here: see
            // `nothing_draws_the_viewer`).
            #[cfg(all(target_family = "wasm", not(test)))]
            {
                viewer.encoded = encode(&pixels, size, graphics, id);
            }
        }
        match viewer.encoded.as_mut() {
            Some(encoded) if encoded.size == size && (encoded.shown || !hold) => {
                ViewerContent::Ready(encoded)
            }
            _ => ViewerContent::Preparing,
        }
    }

    /// Nothing here draws a picture into cells (the browser, until phase 4): once the
    /// viewer's pixels are there it says so, instead of preparing the picture for ever.
    #[cfg(any(target_family = "wasm", test))]
    fn nothing_draws_the_viewer(&mut self) {
        if let Some(viewer) = self.viewer.as_mut()
            && matches!(viewer.state, ViewerState::Ready(_))
        {
            viewer.state = ViewerState::Failed(NOT_DRAWN.to_owned());
        }
    }

    /// An encoding for the viewer arrived from its thread.
    #[cfg(any(not(target_family = "wasm"), test))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::decode_avatar;
    use crate::images::testing::*;
    use std::io::Cursor;

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

        // Three sizes are kept; a fourth pushes out the one drawn at longest ago, once
        // neither this frame nor the one before drew it.
        store.begin_frame();
        store.avatar(url, (3, 1), false).unwrap();
        store.avatar(url, (2, 1), false).unwrap();
        store.avatar(url, (3, 1), false).unwrap();
        assert!(store.cleanup_sequence(false).is_empty());
        store.begin_frame();
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

        // The next frames draw all four again: none is made, sent or deleted again,
        // though the first drawn in a frame finds the others drawn in the last.
        for _ in 0..3 {
            store.begin_frame();
            for (&cells, &id) in sizes.iter().zip(&ids) {
                assert_eq!(store.avatar(url, cells, false).unwrap().kitty_id, id);
            }
            assert!(store.cleanup_sequence(false).is_empty());
        }

        // A frame that draws fewer keeps the others, which the frame before drew; in
        // the frame after, the one drawn at longest ago goes.
        store.begin_frame();
        store.avatar(url, (6, 3), false).unwrap();
        assert!(store.cleanup_sequence(false).is_empty());
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

    /// Where nothing draws pictures (the browser), the viewer says so once the picture
    /// is there, and stays so: no "Preparing the picture…" for ever.
    #[test]
    fn where_nothing_draws_the_viewer_says_so_instead_of_preparing() {
        let mut store = ImageStore::new(Graphics::halfblocks());
        let image = remote_image("mxc://x/a", Some((2000, 1000)));
        store.open_viewer(&image);
        store.nothing_draws_the_viewer();
        assert!(matches!(
            store.viewer_content((20, 10), false),
            ViewerContent::Loading
        ));
        store.on_loaded(&image.key, true, Ok(decoded(400, 200)));
        assert_eq!(store.viewer_pixels(), Some((400, 200)));
        for _ in 0..2 {
            store.nothing_draws_the_viewer();
            assert!(matches!(
                store.viewer_content((20, 10), false),
                ViewerContent::Failed(NOT_DRAWN)
            ));
        }
        assert_eq!(store.viewer_pixels(), None);
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
    fn the_pause_before_a_failed_picture_is_asked_for_again_doubles_up_to_a_limit() {
        let minutes = |failures| retry_wait(failures).as_secs() / 60;
        assert_eq!(
            (1..=7).map(minutes).collect::<Vec<_>>(),
            [1, 2, 4, 8, 16, 30, 30]
        );
        assert_eq!(minutes(u32::MAX), 30);
    }

    /// A failed picture is fetched again only once its pause is over and only as it is
    /// drawn again; the pause then doubles. (The clock is the pause's end, set by hand.)
    #[test]
    fn a_failed_custom_emoji_picture_is_tried_again_after_a_growing_pause() {
        let mut store = ImageStore::new(kitty());
        let url = "mxc://example.org/flaky";
        let due = |store: &mut ImageStore| {
            store.emotes.get_mut(url).unwrap().state = SmallState::Failed {
                retry_at: Instant::now(),
            };
        };
        store.begin_frame();
        store.request_emote(url);
        assert_eq!(store.take_emote_requests().len(), 1);
        store.on_emote(url, Err("could not be loaded".into()));
        assert_eq!(store.emotes[url].failures, 1);

        // Within the pause, drawn again and again: nothing is asked for.
        store.begin_frame();
        store.request_emote(url);
        assert!(store.take_emote_requests().is_empty());

        // Over, but not drawn: nothing happens.
        due(&mut store);
        store.begin_frame();
        assert!(store.take_emote_requests().is_empty());
        assert!(matches!(store.emotes[url].state, SmallState::Failed { .. }));

        // Over, and drawn: asked for once, and the name stays while it loads.
        store.request_emote(url);
        assert_eq!(store.take_emote_requests(), [url]);
        assert!(!store.emote_ready(url));
        store.request_emote(url);
        assert!(store.take_emote_requests().is_empty());

        // Failing again waits twice as long.
        store.on_emote(url, Err("could not be loaded".into()));
        assert_eq!(store.emotes[url].failures, 2);
        let SmallState::Failed { retry_at } = store.emotes[url].state else {
            panic!("failed");
        };
        assert!(retry_at > Instant::now() + Duration::from_secs(100));

        // And a later try that works is drawn.
        due(&mut store);
        store.request_emote(url);
        assert_eq!(store.take_emote_requests(), [url]);
        store.on_emote(url, Ok(decoded(64, 64)));
        assert!(store.emote_ready(url));
        assert_eq!(store.emotes[url].failures, 0);
    }

    /// Failed pictures that are all due at once are fetched a few at a time.
    #[test]
    fn failed_pictures_due_at_once_are_fetched_only_a_few_at_a_time() {
        let mut store = ImageStore::new(kitty());
        let urls: Vec<String> = (0..EMOTE_IN_FLIGHT + 3)
            .map(|n| format!("mxc://example.org/e{n}"))
            .collect();
        store.begin_frame();
        for url in &urls {
            store.emotes.insert(
                url.clone(),
                Small {
                    state: SmallState::Failed {
                        retry_at: Instant::now(),
                    },
                    used: 0,
                    failures: 1,
                },
            );
        }
        store.request_emotes(urls.iter().map(String::as_str));
        assert_eq!(store.take_emote_requests().len(), EMOTE_IN_FLIGHT);
    }

    /// A room picture that failed stays failed, whatever time it is.
    #[test]
    fn a_failed_room_picture_is_not_tried_again() {
        let mut store = ImageStore::new(kitty());
        let url = "mxc://example.org/room";
        store.begin_frame();
        assert!(store.request_avatar(url).is_none());
        assert_eq!(store.take_avatar_requests().len(), 1);
        store.on_avatar(url, Err("could not be loaded".into()));
        store.avatars.get_mut(url).unwrap().state = SmallState::Failed {
            retry_at: Instant::now(),
        };
        store.begin_frame();
        assert!(store.request_avatar(url).is_none());
        assert!(store.take_avatar_requests().is_empty());
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
        // A failure is not asked for again at once.
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
