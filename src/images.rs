//! Pictures: what the terminal can draw, how large a picture is on screen, decoding,
//! and the cache of decoded and encoded pictures. The UI thread owns the cache; the
//! Matrix worker only fetches and decodes.
//!
//! With the kitty protocol a picture is sent to the terminal once and then shown through
//! ordinary cells (Unicode placeholders), so scrolling, cropping and popups on top work
//! like for text. Everywhere else pictures are drawn with coloured half blocks.
//!
//! The types, the sizes and decoding are here. Asking the terminal what it can draw is in
//! [`detect`]; the cache, with what the terminal is sent (ratatui-image's encodings,
//! made on a thread of their own for the viewer), is in [`store`].

mod detect;
mod store;

use std::io::Cursor;
use std::sync::Arc;

use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits, Rgba, RgbaImage};

use crate::model::MediaRef;

pub(crate) use detect::{in_multiplexer, placeholders_broken};
pub use store::{EncodeDone, Encoded, ImageStore, ViewerContent};

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
/// Room pictures are kept as squares of this size; they are shown in two cells (four
/// by two in the room list with room previews), which is some 20 to 80 pixels each way.
pub const AVATAR_PIXELS: (u32, u32) = (96, 96);
/// Custom emoji are fetched as thumbnails no larger than this.
pub const EMOTE_PIXELS: (u32, u32) = (64, 64);
/// Columns and rows a custom emoji's picture takes in a line of text or on a reaction:
/// the width of an ordinary emoji, so lines keep their layout.
pub const EMOTE_CELLS: (u16, u16) = (2, 1);
/// Custom emoji pictures on their way at once. A screen full of new ones is fetched a
/// few at a time, the rest in later frames.
pub const EMOTE_IN_FLIGHT: usize = 6;

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
