//! Recording a voice message with programs the system already has. One task per
//! recording, on the async threads; it ends with the recording.
//!
//! Two programs run, with this task between them:
//!
//! ```text
//! capture --raw sound--> this task --raw sound--> encoder --Ogg/Opus--> this task
//! ```
//!
//! The capture program writes what the microphone hears as plain numbers (16 bits,
//! little-endian, one channel, 48000 a second); the encoder turns that into Ogg/Opus.
//! Because the sound passes through here, its length and its waveform are counted from
//! the sound itself (no audio library is linked for that either), the app can show
//! whether the microphone hears anything, and stopping needs nothing of either program
//! but to end: the capture is told to, its end closes the encoder's input, and the
//! encoder writes its last page and ends by itself.
//!
//! Nothing is on disk while the recording runs. What the encoder wrote is held in
//! memory (a few kilobytes a second, bounded) and written once, at the end, to an
//! owner-only file ([`TempAudio::voice`]) that waits to be sent or thrown away. A
//! recording that is given up leaves nothing.
//!
//! Neither program outlives the app: see `child::Group`.
//!
//! What is asked and told, and the counting of the sound, are here; running the two
//! programs is in [`run`].

mod run;

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

use super::recorder::Tool;
use super::{MAX_AMPLITUDE, TempAudio};

pub use run::run;

/// Samples a second, of the sound between the two programs.
pub const RATE: u32 = 48_000;
/// The loudest sample of each stretch this long is one value of the waveform, before
/// it is thinned out: a twentieth of a second.
const WINDOW: u32 = RATE / 20;
/// The most values of a waveform sent with a voice message.
pub const WAVE_VALUES: usize = 100;
/// The waveform is drawn against the loudest moment of the recording, but never
/// against less than this (of 32767): a recording of near silence is not blown up
/// into a loud one.
const QUIET: u16 = 1024;
/// The bounds of a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Sound must arrive within this of the start.
    pub start: Duration,
    /// A recording stops by itself at this length and is kept.
    pub longest: Duration,
    /// A shorter one is not kept.
    pub shortest: Duration,
    /// The most bytes of Ogg/Opus kept.
    pub bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(3),
            longest: Duration::from_secs(5 * 60),
            shortest: Duration::from_millis(500),
            bytes: 8 << 20,
        }
    }
}

/// What the app asks of a running recording. Its end of the channel going (quitting,
/// a panic that unwinds) is [`RecordControl::Cancel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordControl {
    /// End it and keep what was recorded.
    Stop,
    /// End it and keep nothing.
    Cancel,
}

/// Why there is no recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// One of the two programs could not be started.
    Start,
    /// No sound arrived in time.
    NoSound,
    /// The capture ended before it gave any sound.
    Capture,
    /// The encoder ended early, took no sound, or ended with an error.
    Encoder,
    /// What the encoder wrote is not Ogg/Opus.
    NotOpus,
    TooShort,
    TooLarge,
    /// The file could not be written.
    NotKept,
}

/// A finished recording, waiting in a file of its own.
pub struct Recording {
    /// Removed when this is dropped.
    pub file: TempAudio,
    pub duration: Duration,
    /// At most [`WAVE_VALUES`] values, each at most [`MAX_AMPLITUDE`].
    pub waveform: Vec<u16>,
    /// The file's size in bytes.
    pub size: u64,
    /// The microphone heard nothing at all.
    pub silent: bool,
    /// It was stopped by the longest length allowed.
    pub full: bool,
}

/// Hand-written: the waveform is the shape of what was said.
impl fmt::Debug for Recording {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Recording")
            .field("size", &self.size)
            .field("silent", &self.silent)
            .field("full", &self.full)
            .finish_non_exhaustive()
    }
}

/// What the app hears of a recording.
#[derive(Debug)]
pub enum RecordEvent {
    /// The first sound arrived: the microphone is heard.
    Started,
    /// How long it runs, and the loudest moment since the last report (0 to
    /// [`MAX_AMPLITUDE`], of the loudest there can be).
    Level {
        elapsed: Duration,
        level: u16,
    },
    /// The last word, one of three: both programs are gone and waited for.
    Done(Recording),
    Failed(Failure),
    Cancelled,
}

/// A [`RecordEvent`] for the recording with this number. Nothing in it names a room.
#[derive(Debug)]
pub struct RecordUpdate {
    pub id: u64,
    pub event: RecordEvent,
}

/// A recording to make.
pub struct RecordRequest {
    /// The app's number for it, carried by every answer.
    pub id: u64,
    /// Writes raw sound to its output (16 bits, little-endian, one channel, [`RATE`]
    /// a second) and ends when told to (SIGTERM).
    pub capture: Tool,
    /// Reads that sound from its input and writes Ogg/Opus to its output.
    pub encoder: Tool,
    /// Where the finished recording is put.
    pub dir: PathBuf,
    pub limits: Limits,
    /// Closed by the app (dropped): cancel.
    pub control: UnboundedReceiver<RecordControl>,
}

/// Hand-written: the path names the user's runtime folder.
impl fmt::Debug for RecordRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordRequest")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Whether `bytes` begin as an Ogg stream of Opus does: a first page that holds the
/// Opus header and nothing else.
fn is_ogg_opus(bytes: &[u8]) -> bool {
    // The page's header is 27 bytes and, for one packet, one more; then the packet.
    bytes.len() >= 36 && bytes.starts_with(b"OggS") && &bytes[28..36] == b"OpusHead"
}

/// Counts the sound that passes: how much, and how loud.
struct Meter {
    /// The first byte of a sample whose second has not come yet.
    half: Option<u8>,
    samples: u64,
    /// No sample beyond this many is counted or passed on.
    limit: u64,
    /// The loudest sample of each finished [`WINDOW`], and of the one that is filling.
    windows: Vec<u16>,
    window: u16,
    in_window: u32,
    /// The loudest since the app was last told.
    level: u16,
    loudest: u16,
}

impl Meter {
    fn new(longest: Duration) -> Self {
        let limit = (longest.as_millis() as u64).saturating_mul(u64::from(RATE)) / 1000;
        Self {
            half: None,
            samples: 0,
            limit,
            windows: Vec::new(),
            window: 0,
            in_window: 0,
            level: 0,
            loudest: 0,
        }
    }

    /// Takes in what a read brought, which may end in the middle of a sample, and
    /// appends the whole samples that still count to `out`.
    fn take(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        let mut bytes = bytes;
        if let Some(low) = self.half.take() {
            match bytes.split_first() {
                Some((high, rest)) => {
                    self.sample([low, *high], out);
                    bytes = rest;
                }
                None => {
                    self.half = Some(low);
                    return;
                }
            }
        }
        let (pairs, rest) = bytes.as_chunks::<2>();
        for pair in pairs {
            self.sample(*pair, out);
        }
        if let [last] = rest {
            self.half = Some(*last);
        }
    }

    fn sample(&mut self, raw: [u8; 2], out: &mut Vec<u8>) {
        if self.samples >= self.limit {
            return;
        }
        out.extend_from_slice(&raw);
        self.samples += 1;
        let loud = i16::from_le_bytes(raw).unsigned_abs().min(i16::MAX as u16);
        self.window = self.window.max(loud);
        self.level = self.level.max(loud);
        self.loudest = self.loudest.max(loud);
        self.in_window += 1;
        if self.in_window == WINDOW {
            self.windows.push(self.window);
            self.window = 0;
            self.in_window = 0;
        }
    }

    fn full(&self) -> bool {
        self.samples >= self.limit
    }

    fn elapsed(&self) -> Duration {
        Duration::from_nanos(self.samples.saturating_mul(1_000_000_000) / u64::from(RATE))
    }

    /// The loudest moment since this was last asked, of the loudest there can be.
    fn take_level(&mut self) -> u16 {
        let level = scaled(self.level, i16::MAX as u16);
        self.level = 0;
        level
    }

    /// The waveform to send: at most [`WAVE_VALUES`] values, against the loudest
    /// moment of the recording.
    fn waveform(&self) -> Vec<u16> {
        let mut values = self.windows.clone();
        if self.in_window > 0 {
            values.push(self.window);
        }
        if values.len() > WAVE_VALUES {
            values = super::peaks(&values, WAVE_VALUES);
        }
        let top = self.loudest.max(QUIET);
        values.into_iter().map(|value| scaled(value, top)).collect()
    }
}

/// `value` of `top`, as a waveform's value.
fn scaled(value: u16, top: u16) -> u16 {
    let part = u32::from(value) * u32::from(MAX_AMPLITUDE) / u32::from(top.max(1));
    part.min(u32::from(MAX_AMPLITUDE)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the stand-in encoder writes before the sound it is given: the start of an
    /// Ogg stream of Opus, as far as it is looked at here.
    pub(super) const HEAD: &str = "OggS000000000000000000000000OpusHead";

    #[test]
    fn a_sample_split_between_two_reads_is_counted_once() {
        let mut meter = Meter::new(Duration::from_secs(1));
        let mut out = Vec::new();
        // Three samples: 1, -2 and 300, handed over in odd pieces.
        let bytes = [1u8, 0, 0xfe, 0xff, 0x2c, 0x01];
        meter.take(&bytes[..1], &mut out);
        assert_eq!((meter.samples, out.len()), (0, 0));
        meter.take(&[], &mut out);
        meter.take(&bytes[1..4], &mut out);
        assert_eq!(meter.samples, 2);
        meter.take(&bytes[4..5], &mut out);
        meter.take(&bytes[5..], &mut out);
        assert_eq!(meter.samples, 3);
        assert_eq!(out, bytes);
        assert_eq!(meter.loudest, 300);
    }

    #[test]
    fn nothing_beyond_the_longest_length_is_counted_or_passed_on() {
        let mut meter = Meter::new(Duration::from_millis(1));
        let mut out = Vec::new();
        assert!(!meter.full());
        meter.take(&[9u8; 1000], &mut out);
        assert!(meter.full());
        assert_eq!(meter.samples, 48);
        assert_eq!(out.len(), 96);
        assert_eq!(meter.elapsed(), Duration::from_millis(1));
    }

    #[test]
    fn the_waveform_is_drawn_against_the_loudest_moment_and_bounded() {
        let mut meter = Meter::new(Duration::from_secs(60));
        let mut out = Vec::new();
        let window = |value: i16| value.to_le_bytes().repeat(WINDOW as usize);
        // A quiet twentieth of a second, a loud one, a negative one, and a bit more.
        meter.take(&window(4000), &mut out);
        meter.take(&window(16000), &mut out);
        meter.take(&window(-8000), &mut out);
        meter.take(&2000i16.to_le_bytes(), &mut out);
        assert_eq!(meter.waveform(), [256, 1024, 512, 128]);
        // The level is of the loudest there can be, and asked for anew each time.
        assert_eq!(meter.take_level(), 500);
        assert_eq!(meter.take_level(), 0);

        // Thirty seconds are six hundred stretches: the loudest of each six is kept.
        let mut long = Meter::new(Duration::from_secs(60));
        for _ in 0..600 {
            long.take(&window(16000), &mut out);
            out.clear();
        }
        assert_eq!(long.waveform(), vec![1024; WAVE_VALUES]);
        assert_eq!(long.elapsed(), Duration::from_secs(30));
    }

    #[test]
    fn near_silence_is_not_blown_up_and_silence_is_flat() {
        let mut meter = Meter::new(Duration::from_secs(60));
        let mut out = Vec::new();
        meter.take(&64i16.to_le_bytes().repeat(WINDOW as usize), &mut out);
        assert_eq!(meter.waveform(), [64]);
        let mut silent = Meter::new(Duration::from_secs(60));
        silent.take(&vec![0u8; 2 * WINDOW as usize], &mut out);
        assert_eq!(silent.waveform(), [0]);
        assert_eq!(silent.loudest, 0);
        // The loudest negative number has no positive twin.
        let mut edge = Meter::new(Duration::from_secs(60));
        edge.take(&i16::MIN.to_le_bytes(), &mut out);
        assert_eq!(edge.waveform(), [1024]);
    }

    #[test]
    fn only_a_stream_that_begins_with_the_opus_header_is_ogg_opus() {
        assert!(is_ogg_opus(HEAD.as_bytes()));
        assert!(is_ogg_opus(format!("{HEAD}more").as_bytes()));
        assert!(!is_ogg_opus(&HEAD.as_bytes()[..35]));
        assert!(!is_ogg_opus(
            HEAD.replace("OpusHead", "\u{1}vorbis\0").as_bytes()
        ));
        assert!(!is_ogg_opus(HEAD.replace("OggS", "RIFF").as_bytes()));
        assert!(!is_ogg_opus(b""));
    }
}
