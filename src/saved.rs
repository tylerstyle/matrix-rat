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
    /// The account whose list this is: another account of the same profile must
    /// not see it.
    owner: &'a str,
    items: &'a [SavedItem],
}

/// What went wrong while the file was read. The app starts in every case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadProblem {
    /// The file could not be read at all, or is not a list of ours. It was put aside
    /// (if `kept`), and the list starts empty.
    Damaged { kept: bool },
    /// The file is the list of another account (or names none). It was put aside
    /// (if `kept`), and the list starts empty.
    OtherAccount { kept: bool },
    /// A later version of the app wrote the file. It stays as it is; the list is
    /// empty and takes nothing.
    Newer,
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
                "The saved messages file could not be read: nothing can be saved".to_owned()
            }
            Self::OtherAccount { kept: true } => format!(
                "The saved messages file is another account's: the list starts empty (the old file is kept as {OTHER_NAME})"
            ),
            Self::OtherAccount { kept: false } => {
                "The saved messages file is another account's: nothing can be saved".to_owned()
            }
            Self::Newer => {
                "The saved messages were written by a newer matrix-rat: they are not shown, and nothing can be saved"
                    .to_owned()
            }
            Self::Skipped(1) => "One saved message could not be read and was left out".to_owned(),
            Self::Skipped(count) => {
                format!("{count} saved messages could not be read and were left out")
            }
        }
    }
}

const DAMAGED_NAME: &str = "saved.json.damaged";
/// The list of another account, put aside.
const OTHER_NAME: &str = "saved.json.other-account";
/// The file as it was before a write that drops entries which could not be read.
const BEFORE_NAME: &str = "saved.json.before-repair";
/// Files put aside under one name; beyond that the file stays where it is.
const ASIDE_MOST: usize = 20;

/// Why a message was not saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The list is full.
    Full,
    /// The file is not ours to change (see [`LoadProblem`]).
    Locked,
}

impl Refused {
    pub fn message(self) -> String {
        match self {
            Self::Full => {
                format!("The saved list is full ({MAX_SAVED} messages): remove some first")
            }
            Self::Locked => {
                "Nothing can be saved: the saved messages file is not this matrix-rat's to change"
                    .to_owned()
            }
        }
    }
}

/// The saved messages, newest saved first.
#[derive(Default)]
pub struct SavedStore {
    items: Vec<SavedItem>,
    /// Where the list is kept; `None` for the demo and in tests, which keep it in memory.
    file: Option<PathBuf>,
    /// The account the list belongs to, written into the file.
    owner: String,
    /// The file is there and not ours to replace: nothing is taken, nothing written.
    locked: bool,
    /// Entries of the file were left out: before the first write, which drops them
    /// for good, the file is copied.
    copy_first: bool,
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
            .field("locked", &self.locked)
            .field("announced", &self.announced.len())
            .finish_non_exhaustive()
    }
}

impl SavedStore {
    /// Reads the list of the account `owner` from `file`. A missing file is an empty
    /// list; a damaged one, and the list of another account, is put aside and the
    /// list starts empty; one of a newer version stays untouched; entries that make
    /// no sense are left out. Reminders that are due at `now` count as told: they
    /// came due while the app was not running, and are shown as due without a
    /// notification each.
    pub fn load(file: PathBuf, owner: &str, now: i64) -> (Self, Option<LoadProblem>) {
        let damaged = |file: &Path| LoadProblem::Damaged {
            kept: put_aside(file, DAMAGED_NAME),
        };
        let (items, problem) = match read_file(&file) {
            Ok(None) => (Vec::new(), None),
            Ok(Some(text)) => match parse(&text, owner) {
                Parsed::Items(items, 0) => (items, None),
                Parsed::Items(items, skipped) => (items, Some(LoadProblem::Skipped(skipped))),
                Parsed::NotOurs => (Vec::new(), Some(damaged(&file))),
                Parsed::OtherAccount => {
                    let kept = put_aside(&file, OTHER_NAME);
                    (Vec::new(), Some(LoadProblem::OtherAccount { kept }))
                }
                Parsed::Newer => (Vec::new(), Some(LoadProblem::Newer)),
            },
            Err(_) => (Vec::new(), Some(damaged(&file))),
        };
        let mut store = Self {
            items,
            file: Some(file),
            owner: owner.to_owned(),
            // What could not be moved out of the way is not written over.
            locked: matches!(
                problem,
                Some(
                    LoadProblem::Newer
                        | LoadProblem::Damaged { kept: false }
                        | LoadProblem::OtherAccount { kept: false }
                )
            ),
            copy_first: matches!(problem, Some(LoadProblem::Skipped(_))),
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

    /// The event ids saved in that room.
    pub fn in_room<'a>(&'a self, room_id: &'a str) -> impl Iterator<Item = &'a str> {
        self.items
            .iter()
            .filter(move |item| item.room_id == room_id)
            .map(|item| item.event_id.as_str())
    }

    /// Why nothing more can be saved, if so.
    pub fn refuses(&self) -> Option<Refused> {
        if self.locked {
            Some(Refused::Locked)
        } else if self.items.len() >= MAX_SAVED {
            Some(Refused::Full)
        } else {
            None
        }
    }

    /// Saves a message, at the top. One that is saved already stays as it is.
    pub fn add(&mut self, item: SavedItem) -> Result<(), Refused> {
        if self.contains(&item.room_id, &item.event_id) {
            return Ok(());
        }
        if let Some(why) = self.refuses() {
            return Err(why);
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
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.is(room_id, event_id))
        else {
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
        let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.is(room_id, event_id))
        else {
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
    /// the user only. Without a file (the demo) there is nothing to do, and a file
    /// that is not ours to change is left alone.
    pub fn write(&mut self) -> std::io::Result<()> {
        let Some(file) = &self.file else {
            return Ok(());
        };
        if self.locked {
            return Ok(());
        }
        if self.copy_first {
            // Once: what a person might still repair by hand is kept. Without the
            // copy the entries are not dropped either.
            let copy = free_name(file, BEFORE_NAME)
                .ok_or_else(|| std::io::Error::other("no name left for the copy"))?;
            std::fs::copy(file, copy)?;
            self.copy_first = false;
        }
        let out = FileOut {
            version: VERSION,
            owner: &self.owner,
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
    opened.take(MAX_FILE_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(std::io::Error::other("too large"));
    }
    Ok(Some(text))
}

/// What the file's text is.
enum Parsed {
    /// The entries, and how many were left out.
    Items(Vec<SavedItem>, usize),
    /// Not a list of ours at all.
    NotOurs,
    /// A list, of another account or of none.
    OtherAccount,
    /// A list in a form this version does not know.
    Newer,
}

/// Reads the file's text as the list of `owner`. Each entry is read on its own, so
/// that one bad entry costs only itself.
fn parse(text: &str, owner: &str) -> Parsed {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Parsed::NotOurs;
    };
    let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else {
        return Parsed::NotOurs;
    };
    // Before anything else of it is believed: its fields may mean something else.
    if version > VERSION {
        return Parsed::Newer;
    }
    let Some(entries) = value.get("items").and_then(serde_json::Value::as_array) else {
        return Parsed::NotOurs;
    };
    if value.get("owner").and_then(serde_json::Value::as_str) != Some(owner) {
        return Parsed::OtherAccount;
    }
    let mut items: Vec<SavedItem> = Vec::new();
    let mut skipped = 0;
    for entry in entries {
        match serde_json::from_value::<SavedItem>(entry.clone()) {
            Ok(item)
                if item.plausible()
                    && items.len() < MAX_SAVED
                    && !items
                        .iter()
                        .any(|known| known.is(&item.room_id, &item.event_id)) =>
            {
                items.push(item);
            }
            _ => skipped += 1,
        }
    }
    Parsed::Items(items, skipped)
}

/// Moves a file that cannot be used out of the way under `name`, so that the next
/// write does not destroy what a person might still repair. True if it was moved.
fn put_aside(file: &Path, name: &str) -> bool {
    free_name(file, name).is_some_and(|aside| std::fs::rename(file, aside).is_ok())
}

/// `name` next to `file`, or `name.1`, `name.2`…: the first that is not taken, so
/// that what was put aside earlier is not written over.
fn free_name(file: &Path, name: &str) -> Option<PathBuf> {
    (0..ASIDE_MOST)
        .map(|n| match n {
            0 => file.with_file_name(name),
            n => file.with_file_name(format!("{name}.{n}")),
        })
        .find(|candidate| !candidate.exists())
}

/// Removes the list and everything put aside next to it: for a logout, after which
/// nothing of the account's may stay. `file` is where the list is kept.
pub fn remove_files(file: &Path) {
    let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let ours = entry
            .file_name()
            .to_str()
            .is_some_and(|found| found == name || found.starts_with(&format!("{name}.")));
        if ours {
            let _ = std::fs::remove_file(entry.path());
        }
    }
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
        // On the disk before it takes the old file's place: a crash leaves one of
        // the two whole.
        file.sync_all()?;
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

    /// The moment `when` means, seen from now (see [`resolve`]).
    pub fn resolve(self, when: When) -> Result<i64, WhenError> {
        match self {
            Self::System => resolve(when, self.now_secs(), &Local),
            #[cfg(test)]
            Self::Fixed(now) => resolve(when, now.timestamp(), &now.timezone()),
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

/// The moment the wall clock of `zone` shows `local`. `None` for a time that does not
/// exist (the hour skipped when the clocks go forward); of a time that comes twice,
/// the first.
fn moment<Tz: TimeZone>(zone: &Tz, local: NaiveDateTime) -> Option<i64> {
    zone.from_local_datetime(&local)
        .earliest()
        .map(|at| at.timestamp())
}

/// A time for a reminder, as chosen or typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// This many seconds from now: "in 20 minutes" is twenty minutes of waiting,
    /// whatever the clocks do meanwhile.
    After(i64),
    /// When the wall clock shows this: "tomorrow at 9:00", "in a week" (the same
    /// time of day), a date.
    Wall(NaiveDateTime),
}

/// The moment `when` means, in seconds since 1970, seen from `now_secs` in `zone`.
/// Refused: a wall time the zone skips, and what does not lie after now.
pub fn resolve<Tz: TimeZone>(when: When, now_secs: i64, zone: &Tz) -> Result<i64, WhenError> {
    let secs = match when {
        When::After(span) => now_secs.checked_add(span).ok_or(WhenError::TooFar)?,
        When::Wall(local) => moment(zone, local).ok_or(WhenError::Skipped)?,
    };
    // Also for a wall time: where the clocks went back, "later today" on the wall
    // can be a moment that has passed.
    if secs <= now_secs {
        return Err(WhenError::Past);
    }
    // A day of slack for what the clocks do in five years.
    if secs - now_secs > (MAX_AHEAD_DAYS + 1) * 24 * 3600 {
        return Err(WhenError::TooFar);
    }
    Ok(secs)
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

    /// The time it means, where the wall clock shows `now`.
    pub fn when(self, now: NaiveDateTime) -> Option<When> {
        Some(match self {
            Preset::Minutes20 => When::After(20 * 60),
            Preset::Hour1 => When::After(3600),
            Preset::Hours3 => When::After(3 * 3600),
            Preset::Tomorrow => When::Wall(morning(now.date().succ_opt()?)?),
            Preset::Week1 => When::Wall(now.checked_add_signed(Duration::days(7))?),
        })
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
    /// A time the wall clock skips on that day (the clocks go forward).
    Skipped,
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
            Self::Skipped => "That time does not exist on that day (the clocks change)".to_owned(),
            Self::Past => "That time has passed".to_owned(),
            Self::TooFar => "That is more than five years from now".to_owned(),
        }
    }
}

/// The time that `text` means, where the wall clock shows `now`. Understood are:
/// a span (`45m`, `2h`, `3d`, `1w`, also with `in` before and the unit written out),
/// a time of day (`18:30`: today, or tomorrow if that has passed), `tomorrow` with or
/// without a time of day, and a date `2027-01-31` with or without a time of day
/// (without: at 9:00). Minutes and hours are a wait of that length; days and weeks
/// keep the time of day. The result lies after `now` and at most five years ahead
/// (for a wall time [`resolve`] has the last word on both).
pub fn parse_when(text: &str, now: NaiveDateTime) -> Result<When, WhenError> {
    let text = text.trim().to_lowercase();
    let text = text.strip_prefix("in ").unwrap_or(&text).trim();
    let text = text.strip_prefix("at ").unwrap_or(text).trim();
    if text.is_empty() {
        return Err(WhenError::Empty);
    }
    let at = if let Some(span) = parse_span(text) {
        match span? {
            Span::Seconds(seconds) => return Ok(When::After(seconds)),
            Span::Days(days) => now
                .checked_add_signed(Duration::days(days))
                .ok_or(WhenError::TooFar)?,
        }
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
    Ok(When::Wall(at))
}

/// A typed span: a wait, or days on the calendar.
enum Span {
    Seconds(i64),
    Days(i64),
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
fn parse_span(text: &str) -> Option<Result<Span, WhenError>> {
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let unit = text[digits..].trim();
    // Minutes each, and whether the unit counts days on the calendar.
    let (minutes_each, days): (i64, bool) = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => (1, false),
        "h" | "hr" | "hrs" | "hour" | "hours" => (60, false),
        "d" | "day" | "days" => (24 * 60, true),
        "w" | "week" | "weeks" => (7 * 24 * 60, true),
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
        Some(minutes) if minutes <= MAX_AHEAD_DAYS * 24 * 60 => Ok(if days {
            Span::Days(minutes / (24 * 60))
        } else {
            Span::Seconds(minutes * 60)
        }),
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
    /// Boxed: the form with its text field is much larger than nothing.
    Remind(Box<RemindForm>),
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
    /// Entries that could not be loaded while their room was not listed: asked
    /// again once it is.
    pub retry: HashSet<(String, String)>,
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
            retry: HashSet::new(),
        }
    }

    pub fn form(form: RemindForm) -> Self {
        Self {
            stage: Stage::Remind(Box::new(form)),
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
        self.details.retain(|known| {
            !(known.room_id == details.room_id && known.event_id == details.event_id)
        });
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

/// Reminders that came due together and wait for what the worker says about their
/// messages, to show it in their notifications.
pub struct Waiting {
    pub request: u64,
    /// Those the worker has not answered for yet.
    pub items: Vec<SavedItem>,
    pub since: std::time::Instant,
}

/// What the app keeps about saved messages.
#[derive(Default)]
pub struct SavedState {
    pub store: SavedStore,
    pub clock: Clock,
    /// Where the profile keeps the list. Read when a session is up and its account
    /// known; `None` for the demo and in tests, which keep the list in memory.
    pub file: Option<PathBuf>,
    /// Numbers the questions to the worker.
    pub next_request: u64,
    pub waiting: Vec<Waiting>,
    /// Reminder notifications on their way out; the event loop shows them.
    pub notices: Vec<crate::notify::DueReminder>,
    /// The message whose removal was asked about (its reminder still waits), and
    /// when: `(room, event)`.
    pub confirm: Option<((String, String), std::time::Instant)>,
}

/// Hand-written: only counts.
impl std::fmt::Debug for SavedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SavedState")
            .field("store", &self.store)
            .field("file", &self.file.is_some())
            .field("waiting", &self.waiting.len())
            .field("notices", &self.notices.len())
            .field("confirm", &self.confirm.is_some())
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

    const ME: &str = "@me:example.org";

    /// What the wall clock shows at `when`, where it shows `now` and never jumps.
    fn on_wall(when: When, now: NaiveDateTime) -> NaiveDateTime {
        match when {
            When::After(seconds) => now + Duration::seconds(seconds),
            When::Wall(local) => local,
        }
    }

    /// A zone whose clocks jump: one hour east of Greenwich, two in summer. On
    /// 2026-03-29 the wall clock goes from 02:00 to 03:00 (01:00 UTC), on 2026-10-25
    /// from 03:00 back to 02:00 (01:00 UTC).
    #[derive(Debug, Clone, Copy)]
    struct Jumpy;

    impl Jumpy {
        fn summer(utc: &NaiveDateTime) -> bool {
            *utc >= at(2026, 3, 29, 1, 0) && *utc < at(2026, 10, 25, 1, 0)
        }

        fn offset(summer: bool) -> FixedOffset {
            FixedOffset::east_opt(if summer { 7200 } else { 3600 }).unwrap()
        }
    }

    impl TimeZone for Jumpy {
        type Offset = FixedOffset;

        fn from_offset(_: &FixedOffset) -> Self {
            Jumpy
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> chrono::LocalResult<FixedOffset> {
            self.offset_from_local_datetime(&local.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(
            &self,
            local: &NaiveDateTime,
        ) -> chrono::LocalResult<FixedOffset> {
            // An offset fits where the moment it gives has that offset.
            let fits = |summer: bool| {
                let offset = Self::offset(summer);
                let utc = *local - Duration::seconds(i64::from(offset.local_minus_utc()));
                (Self::summer(&utc) == summer).then_some(offset)
            };
            match (fits(true), fits(false)) {
                // The larger offset is the earlier moment.
                (Some(first), Some(second)) => chrono::LocalResult::Ambiguous(first, second),
                (Some(only), None) | (None, Some(only)) => chrono::LocalResult::Single(only),
                (None, None) => chrono::LocalResult::None,
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> FixedOffset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(12, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> FixedOffset {
            Self::offset(Self::summer(utc))
        }
    }

    /// The moment the jumpy zone's wall clock shows that time (the first, if twice).
    fn jumpy(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        moment(&Jumpy, at(y, mo, d, h, mi)).unwrap()
    }

    #[test]
    fn minutes_and_hours_are_a_wait_and_days_keep_the_time_of_day() {
        assert_eq!(Preset::Minutes20.when(now()), Some(When::After(1200)));
        assert_eq!(Preset::Hour1.when(now()), Some(When::After(3600)));
        assert_eq!(Preset::Hours3.when(now()), Some(When::After(10_800)));
        assert_eq!(
            Preset::Tomorrow.when(now()),
            Some(When::Wall(at(2026, 11, 21, 9, 0)))
        );
        assert_eq!(
            Preset::Week1.when(now()),
            Some(When::Wall(at(2026, 11, 27, 14, 30)))
        );
        assert_eq!(parse_when("45m", now()), Ok(When::After(2700)));
        assert_eq!(parse_when("in 2 hours", now()), Ok(When::After(7200)));
        assert_eq!(
            parse_when("3d", now()),
            Ok(When::Wall(at(2026, 11, 23, 14, 30)))
        );
        assert_eq!(
            parse_when("1w", now()),
            Ok(When::Wall(at(2026, 11, 27, 14, 30)))
        );
        assert_eq!(
            parse_when("18:30", now()),
            Ok(When::Wall(at(2026, 11, 20, 18, 30)))
        );
    }

    #[test]
    fn a_wait_is_that_long_across_a_change_of_the_clocks() {
        // 01:30 on the night the clocks go forward: in an hour the wall shows 03:30.
        let start = jumpy(2026, 3, 29, 1, 30);
        let hour = resolve(When::After(3600), start, &Jumpy).unwrap();
        assert_eq!(hour, start + 3600);
        assert_eq!(
            Jumpy.timestamp_opt(hour, 0).unwrap().naive_local(),
            at(2026, 3, 29, 3, 30)
        );
        // 02:30 for the first time on the night they go back: in an hour it is
        // 02:30 again, and twenty minutes are twenty minutes.
        let start = jumpy(2026, 10, 25, 2, 30);
        let hour = resolve(When::After(3600), start, &Jumpy).unwrap();
        assert_eq!(hour, start + 3600);
        assert_eq!(
            Jumpy.timestamp_opt(hour, 0).unwrap().naive_local(),
            at(2026, 10, 25, 2, 30)
        );
        assert_eq!(resolve(When::After(1200), start, &Jumpy), Ok(start + 1200));
    }

    #[test]
    fn a_wall_time_the_clocks_skip_is_refused_and_one_that_comes_twice_is_the_first() {
        let evening = jumpy(2026, 3, 28, 20, 0);
        let skipped = When::Wall(at(2026, 3, 29, 2, 30));
        assert_eq!(resolve(skipped, evening, &Jumpy), Err(WhenError::Skipped));
        assert!(!WhenError::Skipped.message().is_empty());
        // Either side of the gap is there, an hour of the wall clock apart and one
        // second of time.
        let before = resolve(When::Wall(at(2026, 3, 29, 1, 59)), evening, &Jumpy).unwrap();
        let after = resolve(When::Wall(at(2026, 3, 29, 3, 0)), evening, &Jumpy).unwrap();
        assert_eq!(after - before, 60);
        // "Tomorrow at 9:00" over the change is 9:00 on the wall, an hour sooner.
        let morning = resolve(When::Wall(at(2026, 3, 29, 9, 0)), evening, &Jumpy).unwrap();
        assert_eq!(morning - evening, 12 * 3600);

        let evening = jumpy(2026, 10, 24, 20, 0);
        let twice = resolve(When::Wall(at(2026, 10, 25, 2, 30)), evening, &Jumpy).unwrap();
        // The first 02:30, in summer time: six and a half hours on.
        assert_eq!(twice - evening, 6 * 3600 + 1800);
    }

    #[test]
    fn a_wall_time_that_has_passed_as_a_moment_is_refused() {
        // 02:50 for the second time. 02:40 "today" lies ahead on no clock; 02:55 is
        // ahead on the wall, but its first coming (the one that is meant) has passed.
        let second = jumpy(2026, 10, 25, 2, 50) + 3600;
        assert_eq!(
            resolve(When::Wall(at(2026, 10, 25, 2, 55)), second, &Jumpy),
            Err(WhenError::Past)
        );
        assert_eq!(
            resolve(When::After(0), second, &Jumpy),
            Err(WhenError::Past)
        );
        assert_eq!(
            resolve(When::After(-5), second, &Jumpy),
            Err(WhenError::Past)
        );
        assert_eq!(
            resolve(When::After(i64::MAX), second, &Jumpy),
            Err(WhenError::TooFar)
        );
        assert_eq!(
            resolve(When::After(6 * 366 * 86_400), second, &Jumpy),
            Err(WhenError::TooFar)
        );
        assert_eq!(
            resolve(When::Wall(at(2040, 1, 1, 9, 0)), second, &Jumpy),
            Err(WhenError::TooFar)
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("matrix-rat-saved-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_fixed_choices_mean_what_they_say() {
        assert_eq!(
            Preset::Minutes20
                .when(now())
                .map(|when| on_wall(when, now())),
            Some(at(2026, 11, 20, 14, 50))
        );
        assert_eq!(
            Preset::Hour1.when(now()).map(|when| on_wall(when, now())),
            Some(at(2026, 11, 20, 15, 30))
        );
        assert_eq!(
            Preset::Hours3.when(now()).map(|when| on_wall(when, now())),
            Some(at(2026, 11, 20, 17, 30))
        );
        assert_eq!(
            Preset::Tomorrow
                .when(now())
                .map(|when| on_wall(when, now())),
            Some(at(2026, 11, 21, 9, 0))
        );
        assert_eq!(
            Preset::Week1.when(now()).map(|when| on_wall(when, now())),
            Some(at(2026, 11, 27, 14, 30))
        );
        // Late in the evening "tomorrow" is still the next day, and "in 3 hours" too.
        let late = at(2026, 12, 31, 23, 0);
        assert_eq!(
            Preset::Tomorrow.when(late).map(|when| on_wall(when, late)),
            Some(at(2027, 1, 1, 9, 0))
        );
        assert_eq!(
            Preset::Hours3.when(late).map(|when| on_wall(when, late)),
            Some(at(2027, 1, 1, 2, 0))
        );
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
            assert_eq!(
                parse_when(text, now()).map(|when| on_wall(when, now())),
                Ok(wanted),
                "{text}"
            );
        }
    }

    #[test]
    fn a_time_of_day_is_today_or_if_it_has_passed_tomorrow() {
        assert_eq!(
            parse_when("18:30", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 20, 18, 30))
        );
        assert_eq!(
            parse_when("at 18:30", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 20, 18, 30))
        );
        assert_eq!(
            parse_when("8:05", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 21, 8, 5))
        );
        // Exactly now has passed.
        assert_eq!(
            parse_when("14:30", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 21, 14, 30))
        );
        assert_eq!(
            parse_when("14:31", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 20, 14, 31))
        );
        assert_eq!(
            parse_when("0:00", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 21, 0, 0))
        );
    }

    #[test]
    fn tomorrow_and_a_date_take_a_time_of_day_or_mean_the_morning() {
        assert_eq!(
            parse_when("tomorrow", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 21, 9, 0))
        );
        assert_eq!(
            parse_when("Tomorrow 8:00", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 21, 8, 0))
        );
        assert_eq!(
            parse_when("tomorrow at 17:45", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 11, 21, 17, 45))
        );
        assert_eq!(
            parse_when("2026-12-24", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 12, 24, 9, 0))
        );
        assert_eq!(
            parse_when("2026-12-24 10:00", now()).map(|when| on_wall(when, now())),
            Ok(at(2026, 12, 24, 10, 0))
        );
        assert_eq!(
            parse_when("2027-1-5 7:30", now()).map(|when| on_wall(when, now())),
            Ok(at(2027, 1, 5, 7, 30))
        );
        // Today, later.
        assert_eq!(
            parse_when("2026-11-20 23:00", now()).map(|when| on_wall(when, now())),
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
            assert_eq!(
                parse_when(text, now()).map(|when| on_wall(when, now())),
                Err(why),
                "{text:?}"
            );
            assert!(!why.message().is_empty());
        }
    }

    #[test]
    fn a_time_is_shown_by_its_day_seen_from_now() {
        assert_eq!(when_label(at(2026, 11, 20, 9, 5), now()), "today 09:05");
        assert_eq!(when_label(at(2026, 11, 21, 9, 0), now()), "tomorrow 09:00");
        assert_eq!(
            when_label(at(2026, 11, 19, 23, 59), now()),
            "yesterday 23:59"
        );
        assert_eq!(
            when_label(at(2026, 11, 27, 14, 30), now()),
            "2026-11-27 14:30"
        );
        assert_eq!(when_label(at(2025, 1, 2, 3, 4), now()), "2025-01-02 03:04");
    }

    #[test]
    fn the_clock_turns_wall_time_into_moments_and_back() {
        let clock = Clock::Fixed(friday());
        assert_eq!(clock.now_local(), at(2026, 11, 20, 14, 30));
        let secs = clock.now_secs();
        assert_eq!(
            clock.resolve(When::Wall(at(2026, 11, 20, 14, 30))),
            Err(WhenError::Past)
        );
        assert_eq!(
            clock.resolve(When::Wall(at(2026, 11, 20, 15, 30))),
            Ok(secs + 3600)
        );
        assert_eq!(clock.resolve(When::After(60)), Ok(secs + 60));
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
        assert_eq!(store.add(item("$more", 9999)), Err(Refused::Full));
        assert_eq!(store.refuses(), Some(Refused::Full));
        assert_eq!(store.len(), MAX_SAVED);
        assert!(Refused::Full.message().contains("500"));
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
        let (mut store, problem) = SavedStore::load(file.clone(), ME, 1000);
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

        let (back, problem) = SavedStore::load(file, ME, 1000);
        assert_eq!(problem, None);
        assert_eq!(back.items(), store.items());
        assert_eq!(back.items()[1], first);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_holds_ids_and_times_and_nothing_else() {
        let dir = scratch("content");
        let file = dir.join("saved.json");
        let (mut store, _) = SavedStore::load(file.clone(), ME, 0);
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
                "owner": ME,
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
        let (mut store, _) = SavedStore::load(file.clone(), ME, 0);
        store.add(item("$a", 1)).unwrap();
        store.write().unwrap();
        let (back, problem) = SavedStore::load(file, ME, 0);
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
            (
                "no-version",
                r#"{"owner": "@me:example.org", "items": []}"#.to_owned(),
            ),
            (
                "items-not-a-list",
                r#"{"version": 1, "items": "many"}"#.to_owned(),
            ),
            ("too-large", " ".repeat(MAX_FILE_BYTES as usize + 1)),
        ] {
            let dir = scratch(name);
            let file = dir.join("saved.json");
            std::fs::write(&file, &content).unwrap();
            let (mut store, problem) = SavedStore::load(file.clone(), ME, 0);
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
            assert_eq!(SavedStore::load(file, ME, 0).0.len(), 1);
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
        let (mut store, problem) = SavedStore::load(file.clone(), ME, 0);
        assert!(matches!(problem, Some(LoadProblem::Damaged { .. })));
        assert_eq!(store.len(), 0);
        // It was moved out of the way like any other, and the list works.
        store.add(item("$a", 1)).unwrap();
        store.write().unwrap();
        assert!(file.is_file());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_entries_are_left_out_and_the_good_ones_kept() {
        let dir = scratch("entries");
        let file = dir.join("saved.json");
        let long = "x".repeat(MAX_ID_BYTES + 1);
        let text = serde_json::json!({
            "version": 1,
            "owner": ME,
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
        let (store, problem) = SavedStore::load(file.clone(), ME, 0);
        assert_eq!(problem, Some(LoadProblem::Skipped(8)));
        let ids: Vec<&str> = store.items().iter().map(|i| i.event_id.as_str()).collect();
        assert_eq!(ids, ["$good", "$also-good"]);
        // The file is not put aside for that.
        assert!(file.exists());
        assert!(LoadProblem::Skipped(8).message().contains('8'));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_is_copied_before_the_write_that_drops_what_could_not_be_read() {
        let dir = scratch("repair");
        let file = dir.join("saved.json");
        let text = serde_json::json!({
            "version": 1,
            "owner": ME,
            "items": [
                {"room": "!r:example.org", "event": "$good", "saved_at": 5},
                {"room": "!r:example.org", "event": "$odd", "saved_at": "noon"},
            ],
        })
        .to_string();
        std::fs::write(&file, &text).unwrap();
        let (mut store, problem) = SavedStore::load(file.clone(), ME, 0);
        assert_eq!(problem, Some(LoadProblem::Skipped(1)));
        assert!(!dir.join(BEFORE_NAME).exists());
        store.add(item("$a", 1)).unwrap();
        store.write().unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(BEFORE_NAME)).unwrap(),
            text
        );
        // Once: the next write copies nothing.
        store.add(item("$b", 2)).unwrap();
        store.write().unwrap();
        assert!(!dir.join(format!("{BEFORE_NAME}.1")).exists());
        assert_eq!(SavedStore::load(file, ME, 0).0.len(), 3);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_list_of_another_account_is_put_aside_and_never_shown() {
        for (name, owner) in [
            ("other", serde_json::json!("@other:example.org")),
            ("nobody", serde_json::Value::Null),
        ] {
            let dir = scratch(name);
            let file = dir.join("saved.json");
            let text = serde_json::json!({
                "version": 1,
                "owner": owner,
                "items": [{"room": "!r:example.org", "event": "$theirs", "saved_at": 5, "remind_at": 6}],
            })
            .to_string();
            std::fs::write(&file, &text).unwrap();
            let (mut store, problem) = SavedStore::load(file.clone(), ME, 1000);
            assert_eq!(problem, Some(LoadProblem::OtherAccount { kept: true }));
            assert_eq!(store.len(), 0);
            assert_eq!(store.due(1000).count(), 0);
            assert_eq!(std::fs::read_to_string(dir.join(OTHER_NAME)).unwrap(), text);
            // The list is this account's from here on.
            store.add(item("$mine", 1)).unwrap();
            store.write().unwrap();
            let (back, problem) = SavedStore::load(file, ME, 0);
            assert_eq!((back.len(), problem), (1, None));
            let _ = std::fs::remove_dir_all(dir);
        }
        assert!(
            LoadProblem::OtherAccount { kept: true }
                .message()
                .contains(OTHER_NAME)
        );
    }

    #[test]
    fn a_file_of_a_newer_version_stays_untouched_and_the_list_takes_nothing() {
        let dir = scratch("newer");
        let file = dir.join("saved.json");
        // Whatever else it holds, and whoever it names.
        let text = r#"{"version": 2, "owner": "@me:example.org", "entries": {"new": "form"}}"#;
        std::fs::write(&file, text).unwrap();
        let (mut store, problem) = SavedStore::load(file.clone(), ME, 0);
        assert_eq!(problem, Some(LoadProblem::Newer));
        assert_eq!(store.len(), 0);
        assert_eq!(store.add(item("$a", 1)), Err(Refused::Locked));
        assert_eq!(store.refuses(), Some(Refused::Locked));
        store.write().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        assert!(!LoadProblem::Newer.message().is_empty());
        assert!(!Refused::Locked.message().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn what_was_put_aside_earlier_is_not_written_over() {
        let dir = scratch("aside-twice");
        let file = dir.join("saved.json");
        for (n, content) in ["first {", "second {", "third {"].iter().enumerate() {
            std::fs::write(&file, content).unwrap();
            let (_, problem) = SavedStore::load(file.clone(), ME, 0);
            assert_eq!(problem, Some(LoadProblem::Damaged { kept: true }), "{n}");
        }
        let read = |name: String| std::fs::read_to_string(dir.join(name)).unwrap();
        assert_eq!(read(DAMAGED_NAME.to_owned()), "first {");
        assert_eq!(read(format!("{DAMAGED_NAME}.1")), "second {");
        assert_eq!(read(format!("{DAMAGED_NAME}.2")), "third {");
        // With no name left the file stays, and is not written over.
        for n in 3..ASIDE_MOST {
            std::fs::write(dir.join(format!("{DAMAGED_NAME}.{n}")), "x").unwrap();
        }
        std::fs::write(&file, "last {").unwrap();
        let (mut store, problem) = SavedStore::load(file.clone(), ME, 0);
        assert_eq!(problem, Some(LoadProblem::Damaged { kept: false }));
        assert_eq!(store.add(item("$a", 1)), Err(Refused::Locked));
        store.write().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "last {");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_logout_removes_the_list_and_all_that_was_put_aside() {
        let dir = scratch("logout");
        let file = dir.join("saved.json");
        for name in [
            "saved.json",
            "saved.json.new",
            DAMAGED_NAME,
            "saved.json.damaged.1",
            OTHER_NAME,
            BEFORE_NAME,
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        std::fs::write(dir.join("session.json"), "x").unwrap();
        std::fs::write(dir.join("saved.jsonl"), "x").unwrap();
        remove_files(&file);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["saved.jsonl", "session.json"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_with_more_entries_than_the_list_holds_is_cut() {
        let dir = scratch("many");
        let file = dir.join("saved.json");
        let items: Vec<serde_json::Value> = (0..MAX_SAVED + 3)
            .map(|n| serde_json::json!({"room": "!r:example.org", "event": format!("${n}"), "saved_at": n}))
            .collect();
        let text = serde_json::json!({ "version": 1, "owner": ME, "items": items }).to_string();
        std::fs::write(&file, text).unwrap();
        let (store, problem) = SavedStore::load(file, ME, 0);
        assert_eq!(store.len(), MAX_SAVED);
        assert_eq!(problem, Some(LoadProblem::Skipped(3)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reminders_that_came_due_while_the_app_was_away_are_due_and_not_announced_again() {
        let dir = scratch("missed");
        let file = dir.join("saved.json");
        let (mut store, _) = SavedStore::load(file.clone(), ME, 0);
        store.add(item("$missed", 1)).unwrap();
        store.add(item("$later", 2)).unwrap();
        store.add(item("$done", 3)).unwrap();
        store.set_reminder("!room:example.org", "$missed", Some(500));
        store.set_reminder("!room:example.org", "$later", Some(5000));
        store.set_reminder("!room:example.org", "$done", Some(100));
        assert_eq!(store.take_newly_due(100).len(), 1);
        assert!(store.visited("!room:example.org", "$done", 100));
        store.write().unwrap();

        let (mut back, _) = SavedStore::load(file, ME, 1000);
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
        popup
            .asked
            .insert(("!secret-room:example.org".into(), "$secret-event".into()));
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
            file: Some(PathBuf::from("/home/secret/saved.json")),
            waiting: vec![Waiting {
                request: 1,
                items: vec![saved.clone()],
                since: std::time::Instant::now(),
            }],
            notices: vec![crate::notify::DueReminder {
                notice: crate::matrix::Notice {
                    room_id: "!secret-room:example.org".into(),
                    room: "Secret room".into(),
                    sender: "Alice Secret".into(),
                    body: "a secret text".into(),
                    highlight: false,
                    thread: None,
                },
                event: Some(("$secret-event".into(), Some("$secret-root".into()))),
                count: 1,
            }],
            confirm: Some((
                ("!secret-room:example.org".into(), "$secret-event".into()),
                std::time::Instant::now(),
            )),
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
