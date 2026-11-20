//! Saved messages and reminders: what the user marked to come back to, the file it is
//! kept in, and the times a reminder can be set to.
//!
//! The file holds only where a message is (room, event, thread) and times. Nothing a
//! message says is written there: the list asks the worker for the text when it is
//! shown, so message text stays in the session's encrypted store.
//!
//! Nothing here reads the clock: "now" is passed in (see [`Clock`]).

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};

use crate::lineedit::LineEdit;

/// The most messages that can be saved. The list is the user's own, but its file is
/// read at every start and its rows are asked from the worker.
pub const MAX_SAVED: usize = 500;
/// A file larger than this is not ours.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Matrix ids are at most 255 bytes.
const MAX_ID_BYTES: usize = 255;
const VERSION: u64 = 1;
/// The farthest a reminder may lie ahead.
const MAX_AHEAD_DAYS: i64 = 5 * 366;
/// The hour "tomorrow" and a date without a time mean.
const MORNING: (u32, u32) = (9, 0);

// ----- one saved message ---------------------------------------------------------------

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedItem {
    #[serde(rename = "room")]
    pub room_id: String,
    #[serde(rename = "event")]
    pub event_id: String,
    /// The thread it was saved in (its first message's event id), to open it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Seconds since 1970.
    pub saved_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remind_at: Option<i64>,
    /// The reminder came due and the user went to the message.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// Hand-written: which room and which message stay out of logs.
impl std::fmt::Debug for SavedItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedItem")
            .field("reminder", &self.remind_at.is_some())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

/// Where a saved message's reminder stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reminder {
    None,
    /// Set, and not due yet.
    Waiting(i64),
    /// Due, and the user has not gone to the message since.
    Due(i64),
    /// Was due; the user went to the message.
    Done(i64),
}

impl SavedItem {
    pub fn reminder(&self, now: i64) -> Reminder {
        match self.remind_at {
            None => Reminder::None,
            Some(at) if self.done => Reminder::Done(at),
            Some(at) if at <= now => Reminder::Due(at),
            Some(at) => Reminder::Waiting(at),
        }
    }

    pub fn is(&self, room_id: &str, event_id: &str) -> bool {
        self.room_id == room_id && self.event_id == event_id
    }

    /// Whether what was read from the file can be an entry of ours.
    fn plausible(&self) -> bool {
        let id = |id: &str| !id.is_empty() && id.len() <= MAX_ID_BYTES;
        id(&self.room_id)
            && id(&self.event_id)
            && self.thread.as_deref().is_none_or(id)
            && self.saved_at >= 0
            && self.remind_at.is_none_or(|at| at >= 0)
    }
}

// ----- the list and its file -------------------------------------------------------------

/// What the file looks like.
#[derive(Serialize)]
struct FileOut<'a> {
    version: u64,
    items: &'a [SavedItem],
}

/// What went wrong while the file was read. The app starts in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadProblem {
    /// The file could not be read at all, or is not a list of ours. It was put aside
    /// (if `kept`), and the list starts empty.
    Damaged { kept: bool },
    /// This many entries made no sense and were left out.
    Skipped(usize),
}

impl LoadProblem {
    /// For the user.
    pub fn message(self) -> String {
        match self {
            Self::Damaged { kept: true } => format!(
                "The saved messages file was damaged: the list starts empty (the old file is kept as {DAMAGED_NAME})"
            ),
            Self::Damaged { kept: false } => {
                "The saved messages file could not be read: the list starts empty".to_owned()
            }
            Self::Skipped(1) => "One saved message could not be read and was left out".to_owned(),
            Self::Skipped(count) => {
                format!("{count} saved messages could not be read and were left out")
            }
        }
    }
}

const DAMAGED_NAME: &str = "saved.json.damaged";

/// The list is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

impl Full {
    pub fn message(self) -> String {
        format!("The saved list is full ({MAX_SAVED} messages): remove some first")
    }
}

/// The saved messages, newest saved first.
#[derive(Default)]
pub struct SavedStore {
    items: Vec<SavedItem>,
    /// Where the list is kept; `None` for the demo and in tests, which keep it in memory.
    file: Option<PathBuf>,
    /// Due reminders the user was told about in this run (or that were due already
    /// when it started): `(room, event)`.
    announced: HashSet<(String, String)>,
    /// The earliest reminder that is neither due nor announced, so that the check a
    /// few times a second is one comparison.
    next_due: Option<i64>,
    /// Bumped with every change, for what is drawn from the list.
    pub rev: u64,
}

/// Hand-written: the entries name rooms and messages, the path the user's home.
impl std::fmt::Debug for SavedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedStore")
            .field("items", &self.items.len())
            .field("file", &self.file.is_some())
            .field("announced", &self.announced.len())
            .finish_non_exhaustive()
    }
}

impl SavedStore {
    /// Reads the list from `file`. A missing file is an empty list; a damaged one is
    /// put aside and the list starts empty; entries that make no sense are left out.
    /// Reminders that are due at `now` count as told: they came due while the app was
    /// not running, and are shown as due without a notification each.
    pub fn load(file: PathBuf, now: i64) -> (Self, Option<LoadProblem>) {
        let (items, problem) = match read_file(&file) {
            Ok(None) => (Vec::new(), None),
            Ok(Some(text)) => match parse(&text) {
                Some((items, 0)) => (items, None),
                Some((items, skipped)) => (items, Some(LoadProblem::Skipped(skipped))),
                None => (Vec::new(), Some(put_aside(&file))),
            },
            Err(_) => (Vec::new(), Some(put_aside(&file))),
        };
        let mut store = Self {
            items,
            file: Some(file),
            ..Self::default()
        };
        store.announced = store
            .items
            .iter()
            .filter(|item| matches!(item.reminder(now), Reminder::Due(_)))
            .map(|item| (item.room_id.clone(), item.event_id.clone()))
            .collect();
        store.plan();
        (store, problem)
    }

    pub fn items(&self) -> &[SavedItem] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn get(&self, room_id: &str, event_id: &str) -> Option<&SavedItem> {
        self.items.iter().find(|item| item.is(room_id, event_id))
    }

    pub fn contains(&self, room_id: &str, event_id: &str) -> bool {
        self.get(room_id, event_id).is_some()
    }

    pub fn position(&self, room_id: &str, event_id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.is(room_id, event_id))
    }

    /// The event ids saved in that room.
    pub fn in_room<'a>(&'a self, room_id: &'a str) -> impl Iterator<Item = &'a str> {
        self.items
            .iter()
            .filter(move |item| item.room_id == room_id)
            .map(|item| item.event_id.as_str())
    }

    /// Saves a message, at the top. One that is saved already stays as it is.
    pub fn add(&mut self, item: SavedItem) -> Result<(), Full> {
        if self.contains(&item.room_id, &item.event_id) {
            return Ok(());
        }
        if self.items.len() >= MAX_SAVED {
            return Err(Full);
        }
        self.items.insert(0, item);
        self.changed();
        Ok(())
    }

    /// True if it was there.
    pub fn remove(&mut self, room_id: &str, event_id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|item| !item.is(room_id, event_id));
        self.announced
            .remove(&(room_id.to_owned(), event_id.to_owned()));
        let removed = self.items.len() != before;
        if removed {
            self.changed();
        }
        removed
    }

    /// Sets the reminder of a saved message, or takes it away. A new time starts
    /// afresh: it will be announced, and is not done.
    pub fn set_reminder(&mut self, room_id: &str, event_id: &str, at: Option<i64>) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.is(room_id, event_id)) else {
            return false;
        };
        item.remind_at = at;
        item.done = false;
        self.announced
            .remove(&(room_id.to_owned(), event_id.to_owned()));
        self.changed();
        true
    }

    /// The user went to the message: a reminder that was due is done.
    pub fn visited(&mut self, room_id: &str, event_id: &str, now: i64) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.is(room_id, event_id)) else {
            return false;
        };
        if !matches!(item.reminder(now), Reminder::Due(_)) {
            return false;
        }
        item.done = true;
        self.changed();
        true
    }

    /// The reminders that came due since the last look, each given once.
    pub fn take_newly_due(&mut self, now: i64) -> Vec<SavedItem> {
        if self.next_due.is_none_or(|at| at > now) {
            return Vec::new();
        }
        let due: Vec<SavedItem> = self
            .items
            .iter()
            .filter(|item| matches!(item.reminder(now), Reminder::Due(_)))
            .filter(|item| !self.is_announced(item))
            .cloned()
            .collect();
        for item in &due {
            self.announced
                .insert((item.room_id.clone(), item.event_id.clone()));
        }
        self.plan();
        // What is shown as waiting shows as due now.
        self.rev += 1;
        due
    }

    /// The due reminders, in the list's order.
    pub fn due(&self, now: i64) -> impl Iterator<Item = &SavedItem> {
        self.items
            .iter()
            .filter(move |item| matches!(item.reminder(now), Reminder::Due(_)))
    }

    fn is_announced(&self, item: &SavedItem) -> bool {
        // Two short strings cloned for the few reminders that are due.
        self.announced
            .contains(&(item.room_id.clone(), item.event_id.clone()))
    }

    fn changed(&mut self) {
        self.rev += 1;
        self.plan();
    }

    fn plan(&mut self) {
        self.next_due = self
            .items
            .iter()
            .filter(|item| !item.done && !self.is_announced(item))
            .filter_map(|item| item.remind_at)
            .min();
    }

    /// Writes the list to its file, which is replaced in one step and readable by
    /// the user only. Without a file (the demo) there is nothing to do.
    pub fn write(&self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        let out = FileOut {
            version: VERSION,
            items: &self.items,
        };
        let text = serde_json::to_string_pretty(&out).map_err(std::io::Error::other)?;
        write_private(file, text.as_bytes())
    }
}

/// The file's text; `None` if there is no file.
fn read_file(file: &Path) -> std::io::Result<Option<String>> {
    let opened = match std::fs::File::open(file) {
        Ok(opened) => opened,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let mut text = String::new();
    // One byte more than allowed tells a file that is too large from one that fits.
    opened
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(std::io::Error::other("too large"));
    }
    Ok(Some(text))
}

/// The entries of the file's text and how many were left out; `None` if the text is
/// not a list of ours at all. Each entry is read on its own, so that one bad entry
/// costs only itself.
fn parse(text: &str) -> Option<(Vec<SavedItem>, usize)> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let entries = value.get("items")?.as_array()?;
    let mut items: Vec<SavedItem> = Vec::new();
    let mut skipped = 0;
    for entry in entries {
        match serde_json::from_value::<SavedItem>(entry.clone()) {
            Ok(item)
                if item.plausible()
                    && items.len() < MAX_SAVED
                    && !items.iter().any(|known| known.is(&item.room_id, &item.event_id)) =>
            {
                items.push(item);
            }
            _ => skipped += 1,
        }
    }
    Some((items, skipped))
}

/// Moves a file that cannot be used out of the way, so that the next write does not
/// destroy what a person might still repair.
fn put_aside(file: &Path) -> LoadProblem {
    let kept = std::fs::rename(file, file.with_file_name(DAMAGED_NAME)).is_ok();
    LoadProblem::Damaged { kept }
}

/// Writes `bytes` to a new file next to `path`, for the user only, and puts it in
/// `path`'s place in one step.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let temporary = path.with_extension("json.new");
    // What an interrupted write left is not built upon: its mode is not known.
    let _ = std::fs::remove_file(&temporary);
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(bytes)?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

// ----- time ------------------------------------------------------------------------------

/// Where "now" comes from, and how a time on the wall clock becomes a moment.
#[derive(Debug, Clone, Copy, Default)]
pub enum Clock {
    /// The system's clock and time zone.
    #[default]
    System,
    /// A moment that stands still, for tests.
    #[cfg(test)]
    Fixed(DateTime<FixedOffset>),
}

impl Clock {
    fn now(self) -> DateTime<FixedOffset> {
        match self {
            Self::System => Local::now().fixed_offset(),
            #[cfg(test)]
            Self::Fixed(now) => now,
        }
    }

    /// Seconds since 1970.
    pub fn now_secs(self) -> i64 {
        self.now().timestamp()
    }

    /// The date and time the wall clock shows.
    pub fn now_local(self) -> NaiveDateTime {
        self.now().naive_local()
    }

    /// The moment the wall clock shows `local`. `None` for a time that does not exist
    /// (the hour skipped when the clocks go forward); of a time that comes twice, the
    /// first.
    pub fn secs_of(self, local: NaiveDateTime) -> Option<i64> {
        match self {
            Self::System => moment(&Local, local),
            #[cfg(test)]
            Self::Fixed(now) => moment(&now.timezone(), local),
        }
    }

    /// What the wall clock shows at that moment.
    pub fn local_of(self, secs: i64) -> Option<NaiveDateTime> {
        match self {
            Self::System => Local
                .timestamp_opt(secs, 0)
                .single()
                .map(|at| at.naive_local()),
            #[cfg(test)]
            Self::Fixed(now) => now
                .timezone()
                .timestamp_opt(secs, 0)
                .single()
                .map(|at| at.naive_local()),
        }
    }
}

fn moment<Tz: TimeZone>(zone: &Tz, local: NaiveDateTime) -> Option<i64> {
    zone.from_local_datetime(&local)
        .earliest()
        .map(|at| at.timestamp())
}

/// The fixed choices of the reminder form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Minutes20,
    Hour1,
    Hours3,
    Tomorrow,
    Week1,
}

impl Preset {
    pub const ALL: [Preset; 5] = [
        Preset::Minutes20,
        Preset::Hour1,
        Preset::Hours3,
        Preset::Tomorrow,
        Preset::Week1,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Minutes20 => "in 20 minutes",
            Preset::Hour1 => "in 1 hour",
            Preset::Hours3 => "in 3 hours",
            Preset::Tomorrow => "tomorrow at 9:00",
            Preset::Week1 => "in a week",
        }
    }

    /// The time on the wall clock it means, seen from `now`.
    pub fn at(self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        match self {
            Preset::Minutes20 => now.checked_add_signed(Duration::minutes(20)),
            Preset::Hour1 => now.checked_add_signed(Duration::hours(1)),
            Preset::Hours3 => now.checked_add_signed(Duration::hours(3)),
            Preset::Tomorrow => morning(now.date().succ_opt()?),
            Preset::Week1 => now.checked_add_signed(Duration::days(7)),
        }
    }
}

fn morning(day: NaiveDate) -> Option<NaiveDateTime> {
    day.and_hms_opt(MORNING.0, MORNING.1, 0)
}

/// Why a typed time was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenError {
    Empty,
    /// Not one of the forms that are understood.
    Unknown,
    /// A date or a time of day that does not exist (the 31st of June, 25:00).
    NoSuchTime,
    /// Not after now.
    Past,
    TooFar,
}

/// The forms a time can be typed in, for the form's hint and its errors.
pub const WHEN_FORMS: &str = "45m, 2h, 3d, 1w, 18:30, tomorrow 8:00, 2027-01-31 10:00";

impl WhenError {
    pub fn message(self) -> String {
        match self {
            Self::Empty => format!("Type a time: {WHEN_FORMS}"),
            Self::Unknown => format!("Not understood. Try: {WHEN_FORMS}"),
            Self::NoSuchTime => "There is no such date or time".to_owned(),
            Self::Past => "That time has passed".to_owned(),
            Self::TooFar => "That is more than five years from now".to_owned(),
        }
    }
}

/// The time on the wall clock that `text` means, seen from `now`. Understood are:
/// a span (`45m`, `2h`, `3d`, `1w`, also with `in` before and the unit written out),
/// a time of day (`18:30`: today, or tomorrow if that has passed), `tomorrow` with or
/// without a time of day, and a date `2027-01-31` with or without a time of day
/// (without: at 9:00). The result lies after `now` and at most five years ahead.
pub fn parse_when(text: &str, now: NaiveDateTime) -> Result<NaiveDateTime, WhenError> {
    let text = text.trim().to_lowercase();
    let text = text.strip_prefix("in ").unwrap_or(&text).trim();
    let text = text.strip_prefix("at ").unwrap_or(text).trim();
    if text.is_empty() {
        return Err(WhenError::Empty);
    }
    let at = if let Some(span) = parse_span(text) {
        now.checked_add_signed(span?).ok_or(WhenError::TooFar)?
    } else if let Some(rest) = text.strip_prefix("tomorrow") {
        let day = now.date().succ_opt().ok_or(WhenError::TooFar)?;
        on_day(day, rest)?
    } else if let Some(time) = parse_time(text) {
        let today = now.date().and_time(time?);
        if today > now {
            today
        } else {
            today
                .checked_add_signed(Duration::days(1))
                .ok_or(WhenError::TooFar)?
        }
    } else {
        let (date, rest) = match text.split_once(char::is_whitespace) {
            Some((date, rest)) => (date, rest),
            None => (text, ""),
        };
        on_day(parse_date(date).ok_or(WhenError::Unknown)??, rest)?
    };
    if at <= now {
        return Err(WhenError::Past);
    }
    if at - now > Duration::days(MAX_AHEAD_DAYS) {
        return Err(WhenError::TooFar);
    }
    Ok(at)
}

/// `day` at the time of day `rest` names, or in the morning if it names none.
fn on_day(day: NaiveDate, rest: &str) -> Result<NaiveDateTime, WhenError> {
    let rest = rest.trim();
    let rest = rest.strip_prefix("at ").unwrap_or(rest).trim();
    if rest.is_empty() {
        return morning(day).ok_or(WhenError::NoSuchTime);
    }
    Ok(day.and_time(parse_time(rest).ok_or(WhenError::Unknown)??))
}

/// `None` if `text` is not a number and a unit.
fn parse_span(text: &str) -> Option<Result<Duration, WhenError>> {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let unit = text[digits..].trim();
    let minutes_each: i64 = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 1,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60,
        "d" | "day" | "days" => 24 * 60,
        "w" | "week" | "weeks" => 7 * 24 * 60,
        _ => return None,
    };
    // A number too long to be read is a span too long to be wanted.
    let Ok(count) = text[..digits].parse::<i64>() else {
        return Some(Err(WhenError::TooFar));
    };
    if count == 0 {
        return Some(Err(WhenError::Past));
    }
    Some(match count.checked_mul(minutes_each) {
        Some(minutes) if minutes <= MAX_AHEAD_DAYS * 24 * 60 => Ok(Duration::minutes(minutes)),
        _ => Err(WhenError::TooFar),
    })
}

/// `None` if `text` is not `H:MM` or `HH:MM`.
fn parse_time(text: &str) -> Option<Result<chrono::NaiveTime, WhenError>> {
    let (hour, minute) = text.split_once(':')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    if !digits(hour) || hour.len() > 2 || !digits(minute) || minute.len() != 2 {
        return None;
    }
    let (hour, minute) = (hour.parse().ok()?, minute.parse().ok()?);
    Some(chrono::NaiveTime::from_hms_opt(hour, minute, 0).ok_or(WhenError::NoSuchTime))
}

/// `None` if `text` is not `YYYY-MM-DD`.
fn parse_date(text: &str) -> Option<Result<NaiveDate, WhenError>> {
    let mut parts = text.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() != 4 || month.len() > 2 || day.len() > 2 {
        return None;
    }
    let number = |part: &str| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    let (year, month, day) = (number(year)?, number(month)?, number(day)?);
    Some(NaiveDate::from_ymd_opt(year as i32, month, day).ok_or(WhenError::NoSuchTime))
}

/// A time as the list shows it, seen from `now`: "today 14:05", "tomorrow 09:00",
/// "yesterday 08:30", else the date.
pub fn when_label(at: NaiveDateTime, now: NaiveDateTime) -> String {
    let time = at.format("%H:%M");
    match (at.date() - now.date()).num_days() {
        0 => format!("today {time}"),
        1 => format!("tomorrow {time}"),
        -1 => format!("yesterday {time}"),
        _ => format!("{} {time}", at.format("%Y-%m-%d")),
    }
}

// ----- what the worker tells about a saved message ---------------------------------------

/// What can be shown of a saved message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SavedText {
    /// `snippet` is its text.
    Text,
    /// Deleted since.
    Redacted,
    /// Its key is not there.
    Undecryptable,
    /// Could not be loaded (offline, or the server does not give it any more).
    Unavailable,
    /// The account is not in the room any more.
    RoomGone,
}

/// One saved message as the worker found it.
#[derive(Clone, PartialEq, Eq)]
pub struct SavedDetails {
    pub room_id: String,
    pub event_id: String,
    pub sender: String,
    pub sender_id: Option<String>,
    pub snippet: String,
    pub text: SavedText,
}

/// Hand-written: no room, no message, no name, no text.
impl std::fmt::Debug for SavedDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SavedDetails({:?})", self.text)
    }
}

/// What the list shows for the room of a saved message.
pub enum RoomLabel {
    Name(String),
    /// The room list has not arrived yet.
    Unknown,
    /// The account is not in the room any more.
    Gone,
}

// ----- the popup -------------------------------------------------------------------------

/// The form that sets a reminder: the fixed choices, a typed time, and (where there
/// is a reminder) taking it away.
#[derive(Clone)]
pub struct RemindForm {
    pub room_id: String,
    pub event_id: String,
    /// The row the cursor is on: a preset, then the typed time, then "no reminder".
    pub selected: usize,
    pub input: LineEdit,
    pub error: Option<String>,
    /// The message has a reminder, which can be taken away.
    pub has_reminder: bool,
    /// The message is not saved yet: this is saved with the reminder, once a time is
    /// chosen (leaving the form saves nothing).
    pub unsaved: Option<SavedItem>,
}

impl RemindForm {
    /// Bytes the field takes: the longest form is short.
    const INPUT_MAX_BYTES: usize = 64;
    /// The row of the typed time.
    pub const TYPED: usize = Preset::ALL.len();
    /// The row that takes the reminder away.
    pub const CLEAR: usize = Preset::ALL.len() + 1;

    pub fn new(room_id: String, event_id: String, has_reminder: bool) -> Self {
        Self {
            room_id,
            event_id,
            selected: 0,
            input: LineEdit::new().with_max_bytes(Self::INPUT_MAX_BYTES),
            error: None,
            has_reminder,
            unsaved: None,
        }
    }

    pub fn rows(&self) -> usize {
        Self::CLEAR + usize::from(self.has_reminder)
    }

    pub fn step(&mut self, delta: isize) {
        let last = self.rows() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.error = None;
    }

    pub fn select(&mut self, row: usize) {
        if row < self.rows() {
            self.selected = row;
            self.error = None;
        }
    }
}

#[derive(Clone)]
pub enum Stage {
    List,
    Remind(RemindForm),
}

/// The "Saved messages" popup. Its rows are the store's entries; what the worker
/// found about them is kept here while the popup is open, and goes with it.
#[derive(Clone)]
pub struct SavedPopup {
    pub selected: usize,
    pub scroll: usize,
    /// Entries shown as of the last draw; PageUp/PageDown move by it.
    pub page: usize,
    pub reveal: bool,
    pub stage: Stage,
    /// Opened for the reminder form alone (from a message): closing the form closes
    /// the popup instead of showing the list.
    pub form_only: bool,
    pub details: Vec<SavedDetails>,
    /// Entries the worker was asked about and has not answered for: `(room, event)`.
    pub asked: HashSet<(String, String)>,
}

/// Hand-written: rooms, messages, their text and what is typed stay out of logs.
impl std::fmt::Debug for SavedPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedPopup")
            .field("selected", &self.selected)
            .field("form", &matches!(self.stage, Stage::Remind(_)))
            .field("details", &self.details.len())
            .field("asked", &self.asked.len())
            .finish_non_exhaustive()
    }
}

impl SavedPopup {
    pub fn list() -> Self {
        Self {
            selected: 0,
            scroll: 0,
            page: 0,
            reveal: true,
            stage: Stage::List,
            form_only: false,
            details: Vec::new(),
            asked: HashSet::new(),
        }
    }

    pub fn form(form: RemindForm) -> Self {
        Self {
            stage: Stage::Remind(form),
            form_only: true,
            ..Self::list()
        }
    }

    pub fn details_of(&self, room_id: &str, event_id: &str) -> Option<&SavedDetails> {
        self.details
            .iter()
            .find(|known| known.room_id == room_id && known.event_id == event_id)
    }

    /// Takes what the worker found; a newer answer about a message replaces an older.
    pub fn put_details(&mut self, details: SavedDetails) {
        self.asked
            .remove(&(details.room_id.clone(), details.event_id.clone()));
        self.details
            .retain(|known| !(known.room_id == details.room_id && known.event_id == details.event_id));
        // No more than the list can hold.
        if self.details.len() < MAX_SAVED {
            self.details.push(details);
        }
    }

    pub fn step(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        self.reveal = true;
    }

    pub fn select(&mut self, index: usize, len: usize) {
        if index < len {
            self.selected = index;
            self.reveal = true;
        }
    }

    pub fn scroll_by(&mut self, entries: isize) {
        self.scroll = (self.scroll as isize + entries).max(0) as usize;
        self.reveal = false;
    }

    /// Keeps the selection and the scroll position inside a list of `len` entries, of
    /// which `visible` fit.
    pub fn clamp(&mut self, len: usize, visible: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
        if std::mem::take(&mut self.reveal) {
            if self.selected < self.scroll {
                self.scroll = self.selected;
            } else if self.selected >= self.scroll + visible {
                self.scroll = self.selected + 1 - visible;
            }
        }
        self.scroll = self.scroll.min(len.saturating_sub(visible));
    }
}

/// What the app keeps about saved messages.
#[derive(Default)]
pub struct SavedState {
    pub store: SavedStore,
    pub clock: Clock,
    /// Numbers the questions to the worker.
    pub next_request: u64,
    /// Due reminders whose text was asked for, to be shown in their notification:
    /// the request, the message and when it was asked.
    pub waiting: Vec<(u64, SavedItem, std::time::Instant)>,
    /// Reminder notifications on their way out; the event loop shows them.
    pub notices: Vec<crate::matrix::Notice>,
    /// The room's latest notification was a reminder for this message: a click on it
    /// goes there.
    pub clicks: std::collections::HashMap<String, SavedItem>,
}

/// Hand-written: only counts.
impl std::fmt::Debug for SavedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedState")
            .field("store", &self.store)
            .field("waiting", &self.waiting.len())
            .field("notices", &self.notices.len())
            .field("clicks", &self.clicks.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// 2026-11-20 (a Friday) at 14:30, two hours east of Greenwich.
    pub fn friday() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(2 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 20, 14, 30, 0)
            .unwrap()
    }

    fn now() -> NaiveDateTime {
        friday().naive_local()
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    fn item(event: &str, saved_at: i64) -> SavedItem {
        SavedItem {
            room_id: "!room:example.org".into(),
            event_id: event.into(),
            thread: None,
            saved_at,
            remind_at: None,
            done: false,
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "matrix-rat-saved-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_fixed_choices_mean_what_they_say() {
        assert_eq!(Preset::Minutes20.at(now()), Some(at(2026, 11, 20, 14, 50)));
        assert_eq!(Preset::Hour1.at(now()), Some(at(2026, 11, 20, 15, 30)));
        assert_eq!(Preset::Hours3.at(now()), Some(at(2026, 11, 20, 17, 30)));
        assert_eq!(Preset::Tomorrow.at(now()), Some(at(2026, 11, 21, 9, 0)));
        assert_eq!(Preset::Week1.at(now()), Some(at(2026, 11, 27, 14, 30)));
        // Late in the evening "tomorrow" is still the next day, and "in 3 hours" too.
        let late = at(2026, 12, 31, 23, 0);
        assert_eq!(Preset::Tomorrow.at(late), Some(at(2027, 1, 1, 9, 0)));
        assert_eq!(Preset::Hours3.at(late), Some(at(2027, 1, 1, 2, 0)));
    }

    #[test]
    fn a_span_is_counted_from_now() {
        for (text, wanted) in [
            ("45m", at(2026, 11, 20, 15, 15)),
            ("45 min", at(2026, 11, 20, 15, 15)),
            ("in 45 minutes", at(2026, 11, 20, 15, 15)),
            ("2h", at(2026, 11, 20, 16, 30)),
            ("  2 Hours ", at(2026, 11, 20, 16, 30)),
            ("3d", at(2026, 11, 23, 14, 30)),
            ("1 day", at(2026, 11, 21, 14, 30)),
            ("1w", at(2026, 11, 27, 14, 30)),
            ("in 2 weeks", at(2026, 12, 4, 14, 30)),
        ] {
            assert_eq!(parse_when(text, now()), Ok(wanted), "{text}");
        }
    }

    #[test]
    fn a_time_of_day_is_today_or_if_it_has_passed_tomorrow() {
        assert_eq!(parse_when("18:30", now()), Ok(at(2026, 11, 20, 18, 30)));
        assert_eq!(parse_when("at 18:30", now()), Ok(at(2026, 11, 20, 18, 30)));
        assert_eq!(parse_when("8:05", now()), Ok(at(2026, 11, 21, 8, 5)));
        // Exactly now has passed.
        assert_eq!(parse_when("14:30", now()), Ok(at(2026, 11, 21, 14, 30)));
        assert_eq!(parse_when("14:31", now()), Ok(at(2026, 11, 20, 14, 31)));
        assert_eq!(parse_when("0:00", now()), Ok(at(2026, 11, 21, 0, 0)));
    }

    #[test]
    fn tomorrow_and_a_date_take_a_time_of_day_or_mean_the_morning() {
        assert_eq!(parse_when("tomorrow", now()), Ok(at(2026, 11, 21, 9, 0)));
        assert_eq!(parse_when("Tomorrow 8:00", now()), Ok(at(2026, 11, 21, 8, 0)));
        assert_eq!(
            parse_when("tomorrow at 17:45", now()),
            Ok(at(2026, 11, 21, 17, 45))
        );
        assert_eq!(parse_when("2026-12-24", now()), Ok(at(2026, 12, 24, 9, 0)));
        assert_eq!(
            parse_when("2026-12-24 10:00", now()),
            Ok(at(2026, 12, 24, 10, 0))
        );
        assert_eq!(parse_when("2027-1-5 7:30", now()), Ok(at(2027, 1, 5, 7, 30)));
        // Today, later.
        assert_eq!(
            parse_when("2026-11-20 23:00", now()),
            Ok(at(2026, 11, 20, 23, 0))
        );
    }

    #[test]
    fn what_is_not_a_time_is_refused_with_its_reason() {
        for (text, why) in [
            ("", WhenError::Empty),
            ("   ", WhenError::Empty),
            ("in", WhenError::Unknown),
            ("soon", WhenError::Unknown),
            ("5", WhenError::Unknown),
            ("5 parsecs", WhenError::Unknown),
            ("-5m", WhenError::Unknown),
            ("1.5h", WhenError::Unknown),
            ("18:3", WhenError::Unknown),
            ("18:300", WhenError::Unknown),
            ("18:30:00", WhenError::Unknown),
            ("tomorrow morning", WhenError::Unknown),
            ("2026-12-24 noon", WhenError::Unknown),
            ("24.12.2026", WhenError::Unknown),
            ("26-12-24", WhenError::Unknown),
            ("25:00", WhenError::NoSuchTime),
            ("12:60", WhenError::NoSuchTime),
            ("2026-06-31", WhenError::NoSuchTime),
            ("2027-02-29", WhenError::NoSuchTime),
            ("2026-13-01", WhenError::NoSuchTime),
            ("0m", WhenError::Past),
            ("2026-11-20", WhenError::Past),
            ("2026-11-20 14:30", WhenError::Past),
            ("2020-01-01 10:00", WhenError::Past),
            ("9999w", WhenError::TooFar),
            ("99999999999999999999999d", WhenError::TooFar),
            ("2040-01-01", WhenError::TooFar),
        ] {
            assert_eq!(parse_when(text, now()), Err(why), "{text:?}");
            assert!(!why.message().is_empty());
        }
    }

    #[test]
    fn a_time_is_shown_by_its_day_seen_from_now() {
        assert_eq!(when_label(at(2026, 11, 20, 9, 5), now()), "today 09:05");
        assert_eq!(when_label(at(2026, 11, 21, 9, 0), now()), "tomorrow 09:00");
        assert_eq!(when_label(at(2026, 11, 19, 23, 59), now()), "yesterday 23:59");
        assert_eq!(when_label(at(2026, 11, 27, 14, 30), now()), "2026-11-27 14:30");
        assert_eq!(when_label(at(2025, 1, 2, 3, 4), now()), "2025-01-02 03:04");
    }

    #[test]
    fn the_clock_turns_wall_time_into_moments_and_back() {
        let clock = Clock::Fixed(friday());
        assert_eq!(clock.now_local(), at(2026, 11, 20, 14, 30));
        let secs = clock.now_secs();
        assert_eq!(clock.secs_of(at(2026, 11, 20, 14, 30)), Some(secs));
        assert_eq!(clock.secs_of(at(2026, 11, 20, 15, 30)), Some(secs + 3600));
        assert_eq!(clock.local_of(secs + 60), Some(at(2026, 11, 20, 14, 31)));
    }

    #[test]
    fn a_reminder_waits_is_due_and_is_done() {
        let mut saved = item("$a", 100);
        assert_eq!(saved.reminder(1000), Reminder::None);
        saved.remind_at = Some(2000);
        assert_eq!(saved.reminder(1999), Reminder::Waiting(2000));
        assert_eq!(saved.reminder(2000), Reminder::Due(2000));
        saved.done = true;
        assert_eq!(saved.reminder(5000), Reminder::Done(2000));
    }

    #[test]
    fn the_newest_saved_message_is_first_and_none_is_there_twice() {
        let mut store = SavedStore::default();
        store.add(item("$a", 1)).unwrap();
        store.add(item("$b", 2)).unwrap();
        store.add(item("$a", 3)).unwrap();
        let ids: Vec<&str> = store.items().iter().map(|i| i.event_id.as_str()).collect();
        assert_eq!(ids, ["$b", "$a"]);
        assert!(store.contains("!room:example.org", "$a"));
        assert!(!store.contains("!other:example.org", "$a"));
        assert!(store.remove("!room:example.org", "$a"));
        assert!(!store.remove("!room:example.org", "$a"));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn a_full_list_takes_no_more_and_says_so() {
        let mut store = SavedStore::default();
        for n in 0..MAX_SAVED {
            store.add(item(&format!("${n}"), n as i64)).unwrap();
        }
        assert_eq!(store.add(item("$more", 9999)), Err(Full));
        assert_eq!(store.len(), MAX_SAVED);
        assert!(Full.message().contains("500"), "{}", Full.message());
        // One that is saved already is not "more".
        assert_eq!(store.add(item("$3", 9999)), Ok(()));
        assert!(store.remove("!room:example.org", "$3"));
        assert_eq!(store.add(item("$more", 9999)), Ok(()));
    }

    #[test]
    fn a_reminder_is_given_once_when_it_comes_due() {
        let mut store = SavedStore::default();
        store.add(item("$a", 1)).unwrap();
        store.add(item("$b", 2)).unwrap();
        assert!(store.set_reminder("!room:example.org", "$a", Some(2000)));
        assert!(store.set_reminder("!room:example.org", "$b", Some(3000)));
        assert!(!store.set_reminder("!room:example.org", "$none", Some(3000)));
        assert!(store.take_newly_due(1999).is_empty());
        let due = store.take_newly_due(2000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_id, "$a");
        assert!(store.take_newly_due(2500).is_empty());
        assert_eq!(store.due(2500).count(), 1);
        // Both stay due until gone to.
        assert_eq!(store.take_newly_due(3000).len(), 1);
        assert_eq!(store.due(3000).count(), 2);
        assert!(store.visited("!room:example.org", "$a", 3000));
        assert!(!store.visited("!room:example.org", "$a", 3000));
        assert_eq!(store.due(3000).count(), 1);
        assert_eq!(
            store.get("!room:example.org", "$a").unwrap().reminder(3000),
            Reminder::Done(2000)
        );
        // A new time starts afresh.
        assert!(store.set_reminder("!room:example.org", "$a", Some(4000)));
        assert_eq!(
            store.get("!room:example.org", "$a").unwrap().reminder(3000),
            Reminder::Waiting(4000)
        );
        assert_eq!(store.take_newly_due(4000).len(), 1);
        // Taken away: nothing comes due.
        assert!(store.set_reminder("!room:example.org", "$b", None));
        assert_eq!(store.due(9000).count(), 1);
    }

    #[test]
    fn going_to_a_message_whose_reminder_waits_changes_nothing() {
        let mut store = SavedStore::default();
        store.add(item("$a", 1)).unwrap();
        store.set_reminder("!room:example.org", "$a", Some(2000));
        assert!(!store.visited("!room:example.org", "$a", 1000));
        assert_eq!(store.take_newly_due(2000).len(), 1);
    }

    #[test]
    fn the_list_comes_back_from_its_file_which_only_the_user_can_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("roundtrip");
        let file = dir.join("saved.json");
        let (mut store, problem) = SavedStore::load(file.clone(), 1000);
        assert_eq!(problem, None);
        assert_eq!(store.len(), 0);
        let mut first = item("$a", 1);
        first.thread = Some("$root".into());
        store.add(first.clone()).unwrap();
        store.add(item("$b", 2)).unwrap();
        store.set_reminder("!room:example.org", "$b", Some(5000));
        store.write().unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!dir.join("saved.json.new").exists());
        // A second write replaces the first, with the same mode.
        store.write().unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);

        let (back, problem) = SavedStore::load(file, 1000);
        assert_eq!(problem, None);
        assert_eq!(back.items(), store.items());
        assert_eq!(back.items()[1], first);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_holds_ids_and_times_and_nothing_else() {
        let dir = scratch("content");
        let file = dir.join("saved.json");
        let (mut store, _) = SavedStore::load(file.clone(), 0);
        let mut saved = item("$a", 7);
        saved.remind_at = Some(9);
        saved.done = true;
        store.add(saved).unwrap();
        store.write().unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "version": 1,
                "items": [{
                    "room": "!room:example.org",
                    "event": "$a",
                    "saved_at": 7,
                    "remind_at": 9,
                    "done": true,
                }],
            })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_left_by_an_interrupted_write_does_not_stop_the_next_one() {
        let dir = scratch("leftover");
        let file = dir.join("saved.json");
        std::fs::write(dir.join("saved.json.new"), "half").unwrap();
        let (mut store, _) = SavedStore::load(file.clone(), 0);
        store.add(item("$a", 1)).unwrap();
        store.write().unwrap();
        let (back, problem) = SavedStore::load(file, 0);
        assert_eq!((back.len(), problem), (1, None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_damaged_file_is_put_aside_and_the_list_starts_empty() {
        for (name, content) in [
            ("garbage", "this is not json {".to_owned()),
            ("empty", String::new()),
            ("shape", "[1, 2, 3]".to_owned()),
            ("no-items", r#"{"version": 1}"#.to_owned()),
            ("items-not-a-list", r#"{"items": "many"}"#.to_owned()),
            ("too-large", " ".repeat(MAX_FILE_BYTES as usize + 1)),
        ] {
            let dir = scratch(name);
            let file = dir.join("saved.json");
            std::fs::write(&file, &content).unwrap();
            let (mut store, problem) = SavedStore::load(file.clone(), 0);
            assert_eq!(problem, Some(LoadProblem::Damaged { kept: true }), "{name}");
            assert_eq!(store.len(), 0);
            assert!(!file.exists(), "{name}");
            assert_eq!(
                std::fs::read_to_string(dir.join(DAMAGED_NAME)).unwrap(),
                content
            );
            // The list works from here on.
            store.add(item("$a", 1)).unwrap();
            store.write().unwrap();
            assert_eq!(SavedStore::load(file, 0).0.len(), 1);
            let _ = std::fs::remove_dir_all(dir);
        }
        assert!(
            LoadProblem::Damaged { kept: true }
                .message()
                .contains(DAMAGED_NAME)
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_does_not_stop_the_app() {
        let dir = scratch("unreadable");
        // A folder where the file should be.
        let file = dir.join("saved.json");
        std::fs::create_dir_all(&file).unwrap();
        let (store, problem) = SavedStore::load(file, 0);
        assert!(matches!(problem, Some(LoadProblem::Damaged { .. })));
        assert_eq!(store.len(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_entries_are_left_out_and_the_good_ones_kept() {
        let dir = scratch("entries");
        let file = dir.join("saved.json");
        let long = "x".repeat(MAX_ID_BYTES + 1);
        let text = serde_json::json!({
            "version": 1,
            "something_new": true,
            "items": [
                {"room": "!r:example.org", "event": "$good", "saved_at": 5, "later": "field"},
                {"room": "!r:example.org", "event": "$good", "saved_at": 6},
                {"room": "!r:example.org", "saved_at": 5},
                {"room": "", "event": "$empty-room", "saved_at": 5},
                {"room": "!r:example.org", "event": long, "saved_at": 5},
                {"room": "!r:example.org", "event": "$negative", "saved_at": -5},
                {"room": "!r:example.org", "event": "$text-time", "saved_at": "noon"},
                {"room": "!r:example.org", "event": "$bad-reminder", "saved_at": 5, "remind_at": -1},
                "not an entry",
                {"room": "!r:example.org", "event": "$also-good", "saved_at": 4, "remind_at": 10},
            ],
        })
        .to_string();
        std::fs::write(&file, text).unwrap();
        let (store, problem) = SavedStore::load(file.clone(), 0);
        assert_eq!(problem, Some(LoadProblem::Skipped(8)));
        let ids: Vec<&str> = store.items().iter().map(|i| i.event_id.as_str()).collect();
        assert_eq!(ids, ["$good", "$also-good"]);
        // The file is not put aside for that.
        assert!(file.exists());
        assert!(LoadProblem::Skipped(8).message().contains('8'));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_with_more_entries_than_the_list_holds_is_cut() {
        let dir = scratch("many");
        let file = dir.join("saved.json");
        let items: Vec<serde_json::Value> = (0..MAX_SAVED + 3)
            .map(|n| serde_json::json!({"room": "!r:example.org", "event": format!("${n}"), "saved_at": n}))
            .collect();
        std::fs::write(&file, serde_json::json!({ "items": items }).to_string()).unwrap();
        let (store, problem) = SavedStore::load(file, 0);
        assert_eq!(store.len(), MAX_SAVED);
        assert_eq!(problem, Some(LoadProblem::Skipped(3)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reminders_that_came_due_while_the_app_was_away_are_due_and_not_announced_again() {
        let dir = scratch("missed");
        let file = dir.join("saved.json");
        let (mut store, _) = SavedStore::load(file.clone(), 0);
        store.add(item("$missed", 1)).unwrap();
        store.add(item("$later", 2)).unwrap();
        store.add(item("$done", 3)).unwrap();
        store.set_reminder("!room:example.org", "$missed", Some(500));
        store.set_reminder("!room:example.org", "$later", Some(5000));
        store.set_reminder("!room:example.org", "$done", Some(100));
        assert_eq!(store.take_newly_due(100).len(), 1);
        assert!(store.visited("!room:example.org", "$done", 100));
        store.write().unwrap();

        let (mut back, _) = SavedStore::load(file, 1000);
        let due: Vec<&str> = back.due(1000).map(|i| i.event_id.as_str()).collect();
        assert_eq!(due, ["$missed"]);
        assert!(back.take_newly_due(1000).is_empty());
        // The one that was not due yet still fires.
        let fired = back.take_newly_due(5000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].event_id, "$later");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn debug_output_names_no_room_no_message_and_no_text() {
        let mut store = SavedStore::default();
        let mut saved = item("$secret-event", 1);
        saved.room_id = "!secret-room:example.org".into();
        saved.thread = Some("$secret-root".into());
        store.add(saved.clone()).unwrap();
        let mut popup = SavedPopup::form(RemindForm::new(
            "!secret-room:example.org".into(),
            "$secret-event".into(),
            false,
        ));
        if let Stage::Remind(form) = &mut popup.stage {
            form.input.set("secret typing");
        }
        popup.asked.insert(("!secret-room:example.org".into(), "$secret-event".into()));
        let details = SavedDetails {
            room_id: "!secret-room:example.org".into(),
            event_id: "$secret-event".into(),
            sender: "Alice Secret".into(),
            sender_id: Some("@secret:example.org".into()),
            snippet: "a secret text".into(),
            text: SavedText::Text,
        };
        popup.put_details(details.clone());
        let state = SavedState {
            store,
            waiting: vec![(1, saved.clone(), std::time::Instant::now())],
            ..SavedState::default()
        };
        let shown = format!("{saved:?} {popup:?} {details:?} {state:?}");
        assert!(!shown.to_lowercase().contains("secret"), "{shown}");
        assert!(!shown.contains("example.org"), "{shown}");
    }

    #[test]
    fn the_popup_keeps_its_selection_in_the_list_and_in_view() {
        let mut popup = SavedPopup::list();
        popup.step(1, 0);
        assert_eq!(popup.selected, 0);
        popup.step(7, 10);
        popup.clamp(10, 3);
        assert_eq!((popup.selected, popup.scroll), (7, 5));
        popup.step(99, 10);
        popup.clamp(10, 3);
        assert_eq!((popup.selected, popup.scroll), (9, 7));
        popup.scroll_by(-99);
        popup.clamp(10, 3);
        // Scrolled by hand: the selection is not followed.
        assert_eq!((popup.selected, popup.scroll), (9, 0));
        // The list got shorter.
        popup.clamp(2, 3);
        assert_eq!((popup.selected, popup.scroll), (1, 0));
    }

    #[test]
    fn the_form_has_a_row_to_take_a_reminder_away_only_where_there_is_one() {
        let mut form = RemindForm::new("!r".into(), "$e".into(), false);
        assert_eq!(form.rows(), Preset::ALL.len() + 1);
        form.step(99);
        assert_eq!(form.selected, RemindForm::TYPED);
        form.select(RemindForm::CLEAR);
        assert_eq!(form.selected, RemindForm::TYPED);
        let mut form = RemindForm::new("!r".into(), "$e".into(), true);
        form.step(99);
        assert_eq!(form.selected, RemindForm::CLEAR);
        form.step(-99);
        assert_eq!(form.selected, 0);
    }
}
