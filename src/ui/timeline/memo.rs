//! Rows kept from one layout of the open room to the next, message by message.
//!
//! Every change of the open room (a message, a reaction, a receipt, a picture that
//! arrives) has its rows laid out again, and a room holds thousands of messages. Most
//! of them are as they were: their rows are taken over from the layout before, and only
//! the others are laid out anew.
//!
//! Whether a message is as it was is told by a hash of everything its rows are made from
//! ([`hash_inputs`]): all of the message; what it takes from the messages before it (a day
//! divider, its sender's header, a gap, the dot of a further message of a run, a shield
//! said once for a run); whether its spoilers are shown; and what it looks up elsewhere:
//! its picture's place, whether custom emoji are drawn as pictures at all, whether its
//! sender is ignored, whether the room has link previews, the names of the custom emoji it
//! was reacted with, and its notes. What its rows took from the link previews and the
//! custom emoji pictures is known only once they are laid out (which links and emoji they
//! have), so it is hashed apart ([`hash_fetched`]) and looked up again, for the links and
//! emoji the kept rows have. What all messages are laid out with alike (the room, the
//! width, the theme, read markers on or off, a chat with one person or not: [`Shared`]) is
//! not hashed: when any of it changes, nothing is kept. One thing is not of the message nor
//! of the room: the time its code may take to be coloured. A message whose colouring was
//! cut short for lack of it is laid out again each time, until it was not.
//!
//! The hashes are keyed at random, once for each memo: what someone writes cannot be
//! made to hash like something else, which would leave a message showing what it said
//! before.
//!
//! Kept rows are moved, not copied: the memo holds no rows of its own, only where the
//! rows of each message are among the room's. A message that is at another place now
//! (others came or went before it) gets its rows numbered for that place.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::{BuildHasher, Hash, Hasher, RandomState};
use std::mem::discriminant;
use std::ops::Range;

use matrix_sdk::ruma::events::room::MediaSource;
use ratatui::text::Line;

use super::{
    Before, EmoteView, Flow, Lay, Layout, PreviewView, Row, RowCache, RowKey, is_mxc_key,
    message_rows, notes, reaction_label, receipts,
};
use crate::images::Slot;
use crate::model::{
    Attachment, AudioMeta, ImageInfo, ImageSource, ItemRef, MediaRef, Message, PollAnswer,
    PollInfo, Reaction, ReadBy, Reader, Room, RoomKind, ThreadSummary,
};
use crate::notes::{Note, NoteKind, NoteState, Notes};
use crate::previews::Preview;

/// What all messages of a room are laid out with alike: when any of it changes, no
/// rows are kept.
#[derive(PartialEq)]
struct Shared {
    room: String,
    width: usize,
    /// Changes whenever a theme is (re)loaded.
    theme: u64,
    read_markers: bool,
    /// A chat with one person: a read marker says "read".
    direct: bool,
}

/// The notes, and which of them are of their message as it is now.
pub(super) struct NoteView<'a> {
    pub notes: &'a Notes,
    pub current: &'a dyn Fn(&Message, NoteKind, &Note) -> bool,
}

/// What the rows of a room were laid out from, message by message.
pub(super) struct Memo {
    shared: Shared,
    /// The keys of the hashes, the same for the life of the memo.
    keys: RandomState,
    /// One for each message, in their order.
    laid: Vec<Laid>,
    /// How many messages kept their rows in the last layout.
    kept: usize,
}

/// Hand-written: the room's id stays out of logs.
impl fmt::Debug for Memo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Memo")
            .field("messages", &self.laid.len())
            .field("kept", &self.kept)
            .finish_non_exhaustive()
    }
}

/// One message of a layout.
struct Laid {
    /// Its id, hashed: where its rows are looked for.
    id: u64,
    /// See [`hash_inputs`].
    inputs: u64,
    /// See [`hash_fetched`].
    fetched: u64,
    /// The colouring of its code was cut short for lack of time: its rows are not kept,
    /// so that the next layout colours what it can.
    cut_short: bool,
    /// Where its rows, its links and its custom emoji end among those of the room.
    rows: usize,
    wants: usize,
    emote_wants: usize,
}

/// Lays out the rows of `room` as [`RowCache`] holds them, with notes and read
/// markers, keeping those of `previous` that are what they would be laid out as now.
/// `theme`: the theme's revision (see `RowKey`).
pub(super) fn lay_out(
    room: &Room,
    lay: &Lay,
    notes: &NoteView,
    theme: u64,
    read_markers: bool,
    key: RowKey,
    previous: Option<RowCache>,
) -> RowCache {
    let shared = Shared {
        room: room.id.clone(),
        width: lay.max_width,
        theme,
        read_markers,
        direct: room.kind == RoomKind::Person,
    };
    let (mut old, keys, mut out) = match previous {
        Some(cache) if cache.memo.shared == shared => {
            // Room for a few more rows than before: a change seldom adds many.
            let more = |len: usize| len + len / 8;
            let out = Layout {
                rows: Vec::with_capacity(more(cache.rows.len())),
                wants: Vec::with_capacity(more(cache.wants.len())),
                emote_wants: Vec::with_capacity(more(cache.emote_wants.len())),
            };
            let old = Old::new(cache.rows, cache.wants, cache.emote_wants, cache.memo.laid);
            (old, cache.memo.keys, out)
        }
        // Rows laid out otherwise are no use: they go at once. Those of the same room
        // (at another width, say) leave their room to about as many rows again.
        Some(RowCache {
            mut rows,
            mut wants,
            mut emote_wants,
            memo,
            ..
        }) if memo.shared.room == shared.room => {
            rows.clear();
            wants.clear();
            emote_wants.clear();
            let out = Layout {
                rows,
                wants,
                emote_wants,
            };
            (Old::default(), RandomState::new(), out)
        }
        Some(_) | None => (Old::default(), RandomState::new(), Layout::default()),
    };
    let messages = &room.messages;
    let mut laid = Vec::with_capacity(messages.len());
    let noted: HashSet<&str> = notes.notes.events().collect();
    let mut flow = Flow::default();
    let mut feed = Feed {
        keys: &keys,
        bytes: Vec::with_capacity(1024),
    };
    let mut kept = 0;
    for (i, message) in messages.iter().enumerate() {
        let before = flow.next(messages, i);
        let id = keys.hash_one(&message.id);
        let inputs = hash_inputs(&mut feed, message, i, before, lay, notes, &noted);
        let (wants, emote_wants) = (out.wants.len(), out.emote_wants.len());
        let found = old.find(id).filter(|&k| {
            let at = &old.laid[k];
            !old.taken[k] && !at.cut_short && at.inputs == inputs && {
                let (_, wants, emote_wants) = old.ranges(k);
                hash_fetched(&keys, lay, &old.wants[wants], &old.emote_wants[emote_wants])
                    == at.fetched
            }
        });
        let (fetched, cut_short) = match found {
            // Never one that was cut short (see the filter).
            Some(k) => {
                kept += 1;
                old.take(k, i, &mut out);
                (old.laid[k].fetched, false)
            }
            None => {
                let cut_short = fresh(i, message, before, lay, notes, &shared, &mut out);
                let fetched = hash_fetched(
                    &keys,
                    lay,
                    &out.wants[wants..],
                    &out.emote_wants[emote_wants..],
                );
                (fetched, cut_short)
            }
        };
        laid.push(Laid {
            id,
            inputs,
            fetched,
            cut_short,
            rows: out.rows.len(),
            wants: out.wants.len(),
            emote_wants: out.emote_wants.len(),
        });
    }
    // Not much more room than the rows take (as when they were fewer at this width).
    trim(&mut out.rows);
    trim(&mut out.wants);
    trim(&mut out.emote_wants);
    RowCache {
        key,
        rows: out.rows,
        wants: out.wants,
        emote_wants: out.emote_wants,
        memo: Memo {
            shared,
            keys,
            laid,
            kept,
        },
        anchor: None,
    }
}

/// Gives back the room of a list that takes less than half of it.
fn trim<T>(list: &mut Vec<T>) {
    if list.capacity() > 2 * list.len() + 16 {
        list.shrink_to_fit();
    }
}

/// Lays out the message at `i` anew, with its notes and its read marker. Whether the
/// colouring of its code was cut short for lack of time.
fn fresh(
    i: usize,
    message: &Message,
    before: Before,
    lay: &Lay,
    notes: &NoteView,
    shared: &Shared,
    out: &mut Layout,
) -> bool {
    let start = out.rows.len();
    let cut_short = message_rows(i, message, before, lay, out);
    // Transcripts and translations, under the message; a read marker then sits at the
    // end of the last of those rows.
    notes::add_under(
        &mut out.rows,
        start,
        message,
        notes.notes,
        notes.current,
        lay.max_width,
        lay.theme,
    );
    if shared.read_markers
        && let Some(last) = out.rows[start..].last_mut()
    {
        receipts::mark(
            last,
            &message.read_by,
            shared.direct,
            lay.max_width,
            lay.theme,
        );
    }
    cut_short
}

/// The last layout, its rows being taken out of it.
#[derive(Default)]
struct Old {
    rows: Vec<Row>,
    wants: Vec<(usize, String)>,
    emote_wants: Vec<(usize, String)>,
    laid: Vec<Laid>,
    /// The messages whose rows were taken already.
    taken: Vec<bool>,
    /// Where the next message is looked for first: after the last one found.
    next: usize,
    /// The messages by their id, made when first needed.
    by_id: Option<HashMap<u64, usize>>,
}

impl Old {
    fn new(
        rows: Vec<Row>,
        wants: Vec<(usize, String)>,
        emote_wants: Vec<(usize, String)>,
        laid: Vec<Laid>,
    ) -> Self {
        Self {
            rows,
            wants,
            emote_wants,
            taken: vec![false; laid.len()],
            laid,
            next: 0,
            by_id: None,
        }
    }

    /// The message with this id (hashed), if there is one: the one after the last one
    /// found, where messages are in the same order as before, else any.
    fn find(&mut self, id: u64) -> Option<usize> {
        if self.laid.is_empty() {
            return None;
        }
        let found = match self.laid.get(self.next) {
            Some(laid) if laid.id == id => Some(self.next),
            _ => {
                let laid = &self.laid;
                self.by_id
                    .get_or_insert_with(|| {
                        let mut by_id = HashMap::with_capacity(laid.len());
                        for (k, laid) in laid.iter().enumerate() {
                            by_id.entry(laid.id).or_insert(k);
                        }
                        by_id
                    })
                    .get(&id)
                    .copied()
            }
        };
        if let Some(k) = found {
            self.next = k + 1;
        }
        found
    }

    /// Where the rows, links and custom emoji of the message at `k` are.
    fn ranges(&self, k: usize) -> (Range<usize>, Range<usize>, Range<usize>) {
        let (rows, wants, emote_wants) = match k.checked_sub(1) {
            Some(before) => {
                let before = &self.laid[before];
                (before.rows, before.wants, before.emote_wants)
            }
            None => (0, 0, 0),
        };
        let end = &self.laid[k];
        (
            rows..end.rows,
            wants..end.wants,
            emote_wants..end.emote_wants,
        )
    }

    /// Moves the rows, links and custom emoji of the message at `k` to `out`, as those
    /// of the message at `to`.
    fn take(&mut self, k: usize, to: usize, out: &mut Layout) {
        self.taken[k] = true;
        let (rows, wants, emote_wants) = self.ranges(k);
        let moved = k != to;
        out.rows.extend(self.rows[rows].iter_mut().map(|row| {
            let mut row = std::mem::replace(row, Row::plain(Line::default(), None));
            if moved {
                row.renumber(to);
            }
            row
        }));
        out.wants.extend(
            self.wants[wants]
                .iter_mut()
                .map(|(_, url)| (to, std::mem::take(url))),
        );
        out.emote_wants.extend(
            self.emote_wants[emote_wants]
                .iter_mut()
                .map(|(_, url)| (to, std::mem::take(url))),
        );
    }
}

/// A hash of everything the rows of the message at `i` are made from, but for what all
/// messages are laid out with alike ([`Shared`]) and what they took from the link
/// previews and the custom emoji pictures ([`hash_fetched`]). `noted`: the events that
/// have notes.
fn hash_inputs(
    feed: &mut Feed,
    message: &Message,
    i: usize,
    before: Before,
    lay: &Lay,
    notes: &NoteView,
    noted: &HashSet<&str>,
) -> u64 {
    // Every field is named, as in `hash_message`, so that one added later is decided on
    // here.
    let Lay {
        // In `Shared`: the width, and the theme (the text style is the theme's).
        max_width: _,
        theme: _,
        text_style: _,
        // The width less the indent: where a picture goes depends on it.
        body_width,
        revealed,
        images,
        previews,
        emotes,
    } = *lay;
    let EmoteView {
        catalog,
        ignored: no_emotes_from,
    } = *emotes;
    let NoteView {
        notes: by_event,
        current,
    } = *notes;
    feed.bytes.clear();
    let h = &mut *feed;
    hash_message(message, h);
    before.hash(h);
    (revealed == Some(i)).hash(h);
    // The place its picture takes, or the line that says why there is none.
    if let Some(image) = &message.image {
        hash_slot(&images.slot(image, body_width), h);
    }
    images.emote_pictures().hash(h);
    let ignored = |people: &BTreeSet<String>| {
        message
            .sender_id
            .as_deref()
            .is_some_and(|id| people.contains(id))
    };
    ignored(no_emotes_from).hash(h);
    match previews {
        Some(&PreviewView {
            // What its rows took from the previews: `hash_fetched`.
            store: _,
            mode,
            encrypted,
            ignored: no_previews_from,
        }) => {
            1u8.hash(h);
            mode.allows(encrypted).hash(h);
            ignored(no_previews_from).hash(h);
        }
        None => 0u8.hash(h),
    }
    // A reaction with a custom emoji shows its name where the image packs know it.
    for reaction in &message.reactions {
        if is_mxc_key(&reaction.key) {
            reaction_label(&reaction.key, catalog).hash(h);
        }
    }
    if let Some(event) = message.event_id().filter(|event| noted.contains(event)) {
        for (kind, note) in by_event.of(event) {
            kind.hash(h);
            note.hidden.hash(h);
            match &note.state {
                NoteState::Running => 0u8.hash(h),
                NoteState::Done(text) => {
                    1u8.hash(h);
                    text.hash(h);
                }
                NoteState::Failed(why) => {
                    2u8.hash(h);
                    why.hash(h);
                }
            }
            current(message, kind, note).hash(h);
        }
    }
    h.finish()
}

/// A keyed hash of what is written to it, as of [`RandomState`], but collected first and
/// hashed at once: SipHash takes one long slice much faster than the many small writes
/// that hashing a message makes. Numbers take as few bytes as they need (LEB128: most
/// are lengths, counts and the tags of options and enums, one byte each), so there is
/// less to hash; each is still told apart from what follows it, as the standard
/// library's writes are, so different messages give different bytes.
struct Feed<'a> {
    keys: &'a RandomState,
    bytes: Vec<u8>,
}

impl Feed<'_> {
    fn number(&mut self, mut n: u64) {
        while n >= 0x80 {
            self.bytes.push((n & 0x7f) as u8 | 0x80);
            n >>= 7;
        }
        self.bytes.push(n as u8);
    }
}

impl Hasher for Feed<'_> {
    fn write(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn write_u8(&mut self, i: u8) {
        self.bytes.push(i);
    }

    fn write_u16(&mut self, i: u16) {
        self.number(u64::from(i));
    }

    fn write_u32(&mut self, i: u32) {
        self.number(u64::from(i));
    }

    fn write_u64(&mut self, i: u64) {
        self.number(i);
    }

    fn write_usize(&mut self, i: usize) {
        self.number(i as u64);
    }

    fn write_i32(&mut self, i: i32) {
        self.number(u64::from(i.cast_unsigned()));
    }

    fn write_i64(&mut self, i: i64) {
        self.number(i.cast_unsigned());
    }

    fn write_isize(&mut self, i: isize) {
        self.number(i.cast_unsigned() as u64);
    }

    fn finish(&self) -> u64 {
        self.keys.hash_one(self.bytes.as_slice())
    }
}

/// A hash of what rows with these links and custom emoji (`(message, address)` each)
/// took from the link previews and the custom emoji pictures: the preview of each link
/// if there is one, and whether the picture of each emoji is there.
fn hash_fetched(
    keys: &RandomState,
    lay: &Lay,
    wants: &[(usize, String)],
    emote_wants: &[(usize, String)],
) -> u64 {
    if wants.is_empty() && emote_wants.is_empty() {
        return 0;
    }
    let mut state = keys.build_hasher();
    for (_, url) in wants {
        match lay.previews.and_then(|view| view.store.get(url)) {
            Some(Preview { title, description }) => {
                1u8.hash(&mut state);
                title.hash(&mut state);
                description.hash(&mut state);
            }
            None => 0u8.hash(&mut state),
        }
    }
    for (_, url) in emote_wants {
        lay.images.emote_ready(url).hash(&mut state);
    }
    state.finish()
}

fn hash_slot(slot: &Slot, h: &mut impl Hasher) {
    match slot {
        Slot::Off => 0u8.hash(h),
        Slot::Failed(reason) => {
            1u8.hash(h);
            reason.hash(h);
        }
        Slot::Cells(cols, rows) => {
            2u8.hash(h);
            (cols, rows).hash(h);
        }
    }
}

/// Hashes all of `message`. Every field is named, here and in what it holds, so that
/// one added later is decided on here: a field the rows are made from that was left
/// out would keep them showing what the message said before.
fn hash_message(message: &Message, h: &mut impl Hasher) {
    let Message {
        id,
        item,
        kind,
        state,
        sender,
        sender_id,
        sender_ambiguous,
        time,
        day,
        sent_at,
        encrypted,
        body,
        own,
        edited,
        pinned,
        saved,
        html,
        reply_to,
        reply_sender,
        subject,
        reply_event,
        reactions,
        image,
        file,
        thread,
        poll,
        upload,
        send_error,
        shield,
        mentions,
        read_by,
        utd,
    } = message;
    id.hash(h);
    match item {
        None => 0u8.hash(h),
        Some(ItemRef::Event(event)) => {
            1u8.hash(h);
            event.hash(h);
        }
        Some(ItemRef::Transaction(transaction)) => {
            2u8.hash(h);
            transaction.hash(h);
        }
    }
    discriminant(kind).hash(h);
    discriminant(state).hash(h);
    sender.hash(h);
    sender_id.hash(h);
    sender_ambiguous.hash(h);
    time.hash(h);
    day.hash(h);
    sent_at.hash(h);
    (encrypted, own, edited, pinned, saved).hash(h);
    body.hash(h);
    html.hash(h);
    reply_to.hash(h);
    reply_sender.hash(h);
    subject.hash(h);
    reply_event.hash(h);
    reactions.len().hash(h);
    for Reaction { key, count, own } in reactions {
        key.hash(h);
        count.hash(h);
        own.hash(h);
    }
    match image {
        None => 0u8.hash(h),
        Some(image) => {
            1u8.hash(h);
            hash_image(image, h);
        }
    }
    match file {
        None => 0u8.hash(h),
        Some(file) => {
            1u8.hash(h);
            hash_file(file, h);
        }
    }
    match thread {
        None => 0u8.hash(h),
        Some(ThreadSummary {
            replies,
            latest,
            latest_sender,
            unread,
            demo_replies,
        }) => {
            1u8.hash(h);
            replies.hash(h);
            latest.hash(h);
            latest_sender.hash(h);
            unread.hash(h);
            demo_replies.len().hash(h);
            for reply in demo_replies {
                hash_message(reply, h);
            }
        }
    }
    match poll {
        None => 0u8.hash(h),
        Some(PollInfo {
            question,
            disclosed,
            max_selections,
            answers,
            total_voters,
            ended,
            editable,
        }) => {
            1u8.hash(h);
            question.hash(h);
            (disclosed, max_selections, total_voters, ended, editable).hash(h);
            answers.len().hash(h);
            for PollAnswer {
                id,
                text,
                votes,
                own,
            } in answers
            {
                id.hash(h);
                text.hash(h);
                votes.hash(h);
                own.hash(h);
            }
        }
    }
    upload.hash(h);
    send_error.hash(h);
    shield.map(|shield| discriminant(&shield)).hash(h);
    match mentions {
        None => 0u8.hash(h),
        Some(mentions) => {
            1u8.hash(h);
            mentions.user_ids.hash(h);
            mentions.room.hash(h);
        }
    }
    let ReadBy { readers, total } = read_by;
    total.hash(h);
    readers.len().hash(h);
    for Reader {
        name,
        id,
        ambiguous,
        time,
    } in readers
    {
        name.hash(h);
        id.hash(h);
        ambiguous.hash(h);
        time.hash(h);
    }
    utd.map(|why| discriminant(&why)).hash(h);
}

fn hash_image(image: &ImageInfo, h: &mut impl Hasher) {
    let ImageInfo {
        key,
        source,
        name,
        caption,
        dimensions,
        size,
        sticker,
    } = image;
    key.hash(h);
    match source {
        ImageSource::Demo => 0u8.hash(h),
        ImageSource::Matrix(media) => {
            1u8.hash(h);
            let MediaRef { full, thumbnail } = &**media;
            hash_source(full, h);
            match thumbnail {
                None => 0u8.hash(h),
                Some((thumbnail, size)) => {
                    1u8.hash(h);
                    hash_source(thumbnail, h);
                    size.hash(h);
                }
            }
        }
    }
    name.hash(h);
    caption.hash(h);
    dimensions.hash(h);
    size.hash(h);
    sticker.hash(h);
}

fn hash_file(file: &Attachment, h: &mut impl Hasher) {
    let Attachment {
        key,
        name,
        size,
        kind,
        source,
        audio,
    } = file;
    key.hash(h);
    name.hash(h);
    size.hash(h);
    discriminant(kind).hash(h);
    hash_source(source, h);
    match audio.as_deref() {
        None => 0u8.hash(h),
        Some(AudioMeta {
            duration,
            waveform,
            mime,
        }) => {
            1u8.hash(h);
            duration.hash(h);
            waveform.hash(h);
            mime.hash(h);
        }
    }
}

/// Where a file is fetched from (not the key of an encrypted one).
fn hash_source(source: &MediaSource, h: &mut impl Hasher) {
    match source {
        MediaSource::Plain(uri) => {
            0u8.hash(h);
            uri.as_str().hash(h);
        }
        MediaSource::Encrypted(file) => {
            1u8.hash(h);
            file.url.as_str().hash(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use matrix_sdk::ruma::OwnedMxcUri;

    use super::*;
    use crate::config::LinkPreviews;
    use crate::images::ImageStore;
    use crate::images::testing::{decoded, kitty, remote_image};
    use crate::matrix::{CustomEmoji, EmoteCatalog, EmotePack};
    use crate::model::{MediaKind, MsgKind, SendState, Shield};
    use crate::previews::{PreviewStore, Raw};
    use crate::theme::{DEFAULT_THEME, Theme};
    use crate::ui::hit::HitId;
    use crate::ui::timeline::build_layout;

    const BLOB: &str = "mxc://example.org/blob";
    const PARTY: &str = "mxc://example.org/party";
    const LINK: &str = "https://example.org/page";

    /// Everything the rows of a room are laid out from, as a test changes it.
    struct World {
        room: Room,
        width: usize,
        theme: Theme,
        theme_rev: u64,
        revealed: Option<usize>,
        images: ImageStore,
        previews: PreviewStore,
        mode: LinkPreviews,
        ignored: BTreeSet<String>,
        catalog: EmoteCatalog,
        notes: Notes,
        /// Translations are not of their message any more (as when another language
        /// is set).
        stale_translations: bool,
        read_markers: bool,
        /// The last layout.
        cache: Option<RowCache>,
    }

    impl World {
        fn new(messages: Vec<Message>) -> Self {
            let mut room = Room::new("!memo:example.org", "memo", RoomKind::Room);
            room.messages = messages;
            Self {
                room,
                width: 60,
                theme: Theme::builtin(DEFAULT_THEME).unwrap(),
                theme_rev: 0,
                revealed: None,
                images: ImageStore::new(kitty()),
                previews: PreviewStore::default(),
                mode: LinkPreviews::All,
                ignored: BTreeSet::new(),
                catalog: EmoteCatalog::default(),
                notes: Notes::default(),
                stale_translations: false,
                read_markers: true,
                cache: None,
            }
        }

        /// Like `App::note_current`: nothing under a deleted message, and a
        /// translation only of the text it was made from, in the language set.
        fn current(&self, message: &Message, kind: NoteKind, note: &Note) -> bool {
            message.kind != MsgKind::Redacted
                && (kind == NoteKind::Transcript
                    || note.state == NoteState::Running
                    || !(self.stale_translations || message.edited))
        }

        fn preview_view(&self) -> PreviewView<'_> {
            PreviewView {
                store: &self.previews,
                mode: self.mode,
                encrypted: self.room.encrypted,
                ignored: &self.ignored,
            }
        }

        fn emote_view(&self) -> EmoteView<'_> {
            EmoteView {
                catalog: &self.catalog,
                ignored: &self.ignored,
            }
        }

        /// The rows laid out from scratch, as the timeline did before any were kept:
        /// all messages, then their notes, then the read markers.
        fn cold(&self) -> Layout {
            let (previews, emotes) = (self.preview_view(), self.emote_view());
            let layout = build_layout(
                &self.room,
                self.width,
                &self.theme,
                self.revealed,
                &self.images,
                Some(&previews),
                &emotes,
            );
            let current = |m: &Message, kind: NoteKind, note: &Note| self.current(m, kind, note);
            let mut rows = notes::add(
                layout.rows,
                &self.room,
                &self.notes,
                &current,
                self.width,
                &self.theme,
            );
            if self.read_markers {
                rows = receipts::add(rows, &self.room, self.width, &self.theme);
            }
            Layout { rows, ..layout }
        }

        /// Lays the rows out again, keeping what can be kept of the last layout, and
        /// checks them against [`World::cold`]. How many messages kept their rows.
        fn step(&mut self, what: &str) -> usize {
            let previous = self.cache.take();
            let (previews, emotes) = (self.preview_view(), self.emote_view());
            let lay = Lay::new(
                self.width,
                &self.theme,
                self.revealed,
                &self.images,
                Some(&previews),
                &emotes,
            );
            let current = |m: &Message, kind: NoteKind, note: &Note| self.current(m, kind, note);
            let notes = NoteView {
                notes: &self.notes,
                current: &current,
            };
            let cache = lay_out(
                &self.room,
                &lay,
                &notes,
                self.theme_rev,
                self.read_markers,
                RowKey::default(),
                previous,
            );
            same(&cache, &self.cold(), what);
            let kept = cache.memo.kept;
            self.cache = Some(cache);
            kept
        }

        fn len(&self) -> usize {
            self.room.messages.len()
        }

        fn at(&mut self, id: &str) -> &mut Message {
            self.room
                .messages
                .iter_mut()
                .find(|m| m.id == id)
                .expect("a message with that id")
        }

        fn index(&self, id: &str) -> usize {
            self.room.messages.iter().position(|m| m.id == id).unwrap()
        }

        fn picture_arrives(&mut self, key: &str, result: Result<(u32, u32), &str>) {
            let info = remote_image(key, None);
            self.images.begin_frame();
            self.images.request(&info);
            self.images.on_loaded(
                key,
                false,
                result
                    .map(|(w, h)| decoded(w, h))
                    .map_err(|why| why.to_owned()),
            );
        }

        fn emoji_arrives(&mut self, url: &str) {
            self.images.begin_frame();
            self.images.request_emotes([url]);
            self.images.on_emote(url, Ok(decoded(64, 64)));
        }

        fn preview_arrives(&mut self, url: &str, title: &str) {
            self.previews.request(url);
            self.previews.on_loaded(
                url,
                Some(Raw {
                    title: Some(title.to_owned()),
                    site: None,
                    description: Some("What the page says about itself.".to_owned()),
                }),
            );
        }
    }

    /// Fails with the first difference between the kept rows and those laid out cold.
    fn same(kept: &RowCache, cold: &Layout, what: &str) {
        let shown = |row: Option<&Row>| {
            row.map_or("(none)".to_owned(), |row| {
                format!(
                    "{:?} msg {:?} source {} hits {:?} image {:?} emotes {} audio {:?}",
                    row.line.to_string(),
                    row.msg,
                    row.source,
                    row.hits,
                    row.image,
                    row.emotes.len(),
                    row.audio
                )
            })
        };
        let rows = kept.rows.len().max(cold.rows.len());
        if let Some(n) = (0..rows).find(|&n| kept.rows.get(n) != cold.rows.get(n)) {
            panic!(
                "{what}: row {n} of {} (cold: {}) differs\n kept: {}\n cold: {}",
                kept.rows.len(),
                cold.rows.len(),
                shown(kept.rows.get(n)),
                shown(cold.rows.get(n))
            );
        }
        assert_eq!(kept.wants, cold.wants, "{what}: links");
        assert_eq!(kept.emote_wants, cold.emote_wants, "{what}: custom emoji");
    }

    fn message(id: &str, sender: &str, body: &str) -> Message {
        Message {
            id: id.to_owned(),
            item: Some(ItemRef::Event(format!("${id}"))),
            sender: sender.to_owned(),
            sender_id: Some(format!("@{sender}:example.org")),
            time: "12:00".to_owned(),
            day: "Monday".to_owned(),
            body: body.to_owned(),
            own: sender == "you",
            ..Message::default()
        }
    }

    fn reaction(key: &str, count: u32) -> Reaction {
        Reaction {
            key: key.to_owned(),
            count,
            own: false,
        }
    }

    fn readers(names: &[&str]) -> ReadBy {
        ReadBy {
            readers: names
                .iter()
                .map(|name| Reader {
                    name: (*name).to_owned(),
                    time: "12:01".to_owned(),
                    ..Default::default()
                })
                .collect(),
            total: names.len() as u32,
        }
    }

    fn line() -> Message {
        Message {
            id: "line".to_owned(),
            kind: MsgKind::ReadMarker,
            ..Message::default()
        }
    }

    fn party_pack() -> EmotePack {
        EmotePack {
            id: "user".into(),
            name: "Mine".into(),
            emojis: vec![CustomEmoji {
                shortcode: "party".into(),
                url: PARTY.into(),
                body: None,
                pack_name: "Mine".into(),
            }],
            stickers: Vec::new(),
        }
    }

    /// A room with a bit of everything the rows are made from.
    fn sample() -> Vec<Message> {
        let mut code = message("b1", "bob", "fn main() {}");
        code.html = Some("<pre><code class=\"language-rust\">fn main() {}\n</code></pre>".into());
        let mut spoiler = message("b2", "bob", "the end is near");
        spoiler.html = Some("the <span data-mx-spoiler>end</span> is near".into());
        let mut emoji = message("c1", "carol", "hi :blob: there");
        emoji.html = Some(format!(
            "hi <img data-mx-emoticon src=\"{BLOB}\" alt=\":blob:\"> there"
        ));
        emoji.reactions = vec![reaction(PARTY, 2), reaction("👍", 1)];
        let mut picture = message("c2", "carol", "cat.png");
        picture.image = Some(remote_image("mxc://example.org/cat", None));
        let mut sized = message("c3", "carol", "dog.png");
        sized.image = Some(remote_image("mxc://example.org/dog", Some((640, 480))));
        sized.image.as_mut().unwrap().caption = Some("A dog, with its caption".into());
        let mut system = message("s1", "bob", "bob changed the topic");
        system.kind = MsgKind::System;
        let mut reply = message("a3", "alice", "Yes!");
        reply.reply_to = Some(("bob".into(), "the end is near".into()));
        reply.reply_event = Some("$b2".into());
        let mut thread = message("b3", "bob", "Who is in for a thread?");
        thread.thread = Some(ThreadSummary {
            replies: 2,
            latest: Some(("alice".into(), "me".into())),
            latest_sender: None,
            unread: true,
            demo_replies: Vec::new(),
        });
        let mut emote = message("a4", "alice", "waves");
        emote.kind = MsgKind::Emote;
        let mut voice = message("a5", "alice", "🎤 note.ogg · 0:12 · 2 kB\n▁▃▆█");
        voice.file = Some(Attachment {
            key: "mxc://example.org/voice".into(),
            name: "note.ogg".into(),
            size: Some(2000),
            kind: MediaKind::Voice,
            source: Box::new(MediaSource::Plain(OwnedMxcUri::from(
                "mxc://example.org/voice",
            ))),
            audio: Some(Arc::new(AudioMeta {
                duration: Some(std::time::Duration::from_secs(12)),
                waveform: vec![100, 300, 600, 1000],
                mime: None,
            })),
        });
        let mut poll = message("d1", "dave", "📊 Lunch?");
        poll.poll = Some(PollInfo {
            question: "Lunch?".into(),
            disclosed: true,
            max_selections: 1,
            answers: vec![
                PollAnswer {
                    id: "x".into(),
                    text: "Pizza".into(),
                    votes: 2,
                    own: false,
                },
                PollAnswer {
                    id: "y".into(),
                    text: "Soup".into(),
                    votes: 1,
                    own: true,
                },
            ],
            total_voters: 3,
            ended: false,
            editable: false,
        });
        let mut redacted = message("d2", "dave", "Message deleted");
        redacted.kind = MsgKind::Redacted;
        let mut utd = message("d3", "dave", "Unable to decrypt");
        utd.kind = MsgKind::Undecryptable;
        utd.reactions = vec![reaction(PARTY, 1)];
        let mut sending = message("y1", "you", "on my way");
        sending.state = SendState::Sending;
        let mut failed = message("y2", "you", "lost in the post");
        failed.state = SendState::Failed;
        failed.send_error = Some("too large".into());
        let mut bridged = message("e1", "erin", "from the bridge");
        bridged.shield = Some(Shield::SenderMismatch);
        let mut bridged_too = message("e2", "erin", "from the bridge too");
        bridged_too.shield = Some(Shield::SenderMismatch);
        let mut long = message(
            "f1",
            "frank",
            "A new day, and a long message that wraps over more than one row of the \
             pane, and then some more words to be sure.",
        );
        long.day = "Tuesday".into();
        long.edited = true;
        long.pinned = true;
        long.read_by = readers(&["alice", "bob"]);
        let mut full = message("f3", "frank", &"x".repeat(56));
        full.read_by = readers(&["carol"]);
        vec![
            message("a1", "alice", "Good morning"),
            message("a2", "alice", &format!("See {LINK} for the plan")),
            code,
            spoiler,
            emoji,
            picture,
            sized,
            system,
            reply,
            thread,
            emote,
            voice,
            poll,
            redacted,
            utd,
            sending,
            line(),
            failed,
            bridged,
            bridged_too,
            long,
            message("f2", "frank", "and https://example.com/other too"),
            full,
        ]
    }

    #[test]
    fn nothing_changed_keeps_every_message_and_a_first_layout_none() {
        let mut world = World::new(sample());
        assert_eq!(world.step("first"), 0);
        assert_eq!(world.step("again"), world.len());
        // Laid out without a memo, the rows are the same.
        world.cache = None;
        assert_eq!(world.step("forgotten"), 0);
    }

    #[test]
    fn an_edit_a_reaction_or_a_receipt_lays_out_only_its_messages() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();

        let edited = world.at("a2");
        edited.body = "See nothing, the plan changed".into();
        edited.edited = true;
        assert_eq!(world.step("edit"), n - 1);

        world.at("c1").reactions.push(reaction("🎉", 1));
        assert_eq!(world.step("a reaction"), n - 1);
        world.at("c1").reactions[0].count = 3;
        assert_eq!(world.step("one more of a reaction"), n - 1);

        // A receipt moves from one message to another: two messages.
        world.at("f3").read_by = ReadBy::default();
        world.at("a1").read_by = readers(&["carol"]);
        assert_eq!(world.step("a receipt moves"), n - 2);
        // Someone else has read it, and as many people as before.
        world.at("a1").read_by = readers(&["dave"]);
        assert_eq!(world.step("another reader"), n - 1);

        world.at("b3").thread.as_mut().unwrap().replies = 3;
        assert_eq!(world.step("a reply in a thread"), n - 1);
        world.at("y1").state = SendState::Sent;
        assert_eq!(world.step("sent"), n - 1);
        world.at("d1").poll.as_mut().unwrap().answers[0].votes = 3;
        assert_eq!(world.step("a vote"), n - 1);
        let deleted = world.at("a3");
        deleted.kind = MsgKind::Redacted;
        deleted.body = "Message deleted".into();
        deleted.reply_to = None;
        assert_eq!(world.step("deleted"), n - 1);
        world.at("a1").saved = true;
        assert_eq!(world.step("saved"), n - 1);
    }

    #[test]
    fn a_picture_a_preview_or_an_emoji_that_arrives_lays_out_only_its_message() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();
        let rows = world.cache.as_ref().unwrap().rows.len();

        // Its size was not known: it takes the place of its own shape now.
        world.picture_arrives("mxc://example.org/cat", Ok((400, 100)));
        assert_eq!(world.step("a picture"), n - 1);
        assert_ne!(world.cache.as_ref().unwrap().rows.len(), rows);
        // One that cannot be read says so instead.
        world.picture_arrives("mxc://example.org/dog", Err("not a picture"));
        assert_eq!(world.step("a broken picture"), n - 1);

        world.preview_arrives(LINK, "The plan");
        assert_eq!(world.step("a preview"), n - 1);
        assert!(
            world
                .cache
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .any(|row| row.line.to_string().contains("The plan"))
        );

        // The emoji in the text of one message; then the one it and another message
        // (that cannot be decrypted, and shows no pictures) were reacted with.
        world.emoji_arrives(BLOB);
        assert_eq!(world.step("an emoji in a text"), n - 1);
        world.emoji_arrives(PARTY);
        assert_eq!(world.step("an emoji on a reaction"), n - 1);
        // Without pictures the image packs name it.
        world.images = ImageStore::new(crate::images::Graphics::halfblocks());
        world.step("no custom emoji pictures");
        world.catalog.packs.push(party_pack());
        assert_eq!(world.step("image packs"), n - 2);
    }

    #[test]
    fn neighbours_decide_on_dividers_headers_and_shields() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();

        // The first message goes: the second is the first of its day and sender, and
        // every other one is one place up.
        world.room.messages.remove(0);
        assert_eq!(world.step("the first goes"), n - 2);
        // Another sender between two of bob's: his after it gets a header again.
        let at = world.index("b2");
        world
            .room
            .messages
            .insert(at, message("z1", "zed", "an interruption"));
        assert_eq!(world.step("someone in between"), n - 2);
        // A day divider comes and goes with the day of the message before.
        world.at("f1").day = "Monday".into();
        world.step("one day less");
        world.at("f1").day = "Tuesday".into();
        world.step("one day more");
        // The divider names the day.
        world.at("f1").day = "Thursday".into();
        assert_eq!(world.step("another day"), world.len() - 1);
        // The shield is said once for a run of messages from the same sender.
        world.at("e1").shield = None;
        assert_eq!(world.step("a shield goes"), world.len() - 2);
    }

    #[test]
    fn a_namesake_is_someone_else() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();
        // bob's first message is from another bob, on another server: his second one
        // gets a header of its own.
        world.at("b1").sender_id = Some("@bob:elsewhere.org".into());
        assert_eq!(world.step("a namesake"), n - 2);
        // The SDK finds the name ambiguous: that message names its sender anew.
        world.at("b1").sender_ambiguous = true;
        assert_eq!(world.step("told apart"), n - 1);
        assert!(
            rows_of(&world, "b1")
                .iter()
                .any(|row| row.line.to_string().contains("bob (@bob:elsewhere.org)"))
        );
        // The same person again.
        world.at("b1").sender_id = Some("@bob:example.org".into());
        world.at("b1").sender_ambiguous = false;
        assert_eq!(world.step("the same bob"), n - 2);
    }

    #[test]
    fn messages_that_move_get_their_rows_numbered_for_their_place() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();
        let older: Vec<Message> = (0..3)
            .map(|k| message(&format!("o{k}"), "olga", &format!("older {k}")))
            .collect();
        world.room.messages.splice(0..0, older);
        // The first message of before is not the first of the day any more.
        assert_eq!(world.step("history"), n - 1);
        // The reactions of a message that is three places down now are its own.
        let at = world.index("c1");
        let rows = &world.cache.as_ref().unwrap().rows;
        let chips = |m: usize| {
            rows.iter()
                .filter(|row| row.source == m)
                .filter(|row| row.hits.iter().any(|hit| hit.2 == HitId::Reaction(m, 0)))
                .count()
        };
        assert_eq!((chips(at), chips(at - 3)), (1, 0));
    }

    /// The rows of one message, as last laid out.
    fn rows_of(world: &World, id: &str) -> Vec<Row> {
        let at = world.index(id);
        let rows = &world.cache.as_ref().unwrap().rows;
        rows.iter()
            .filter(|row| row.source == at)
            .cloned()
            .collect()
    }

    #[test]
    fn a_message_whose_colours_were_cut_short_is_laid_out_again_until_they_are_not() {
        // No other code: whether another test coloured it already does not matter.
        let mut code = message("k1", "kim", "fn cut_short_for_lack_of_time() {}");
        code.html = Some(
            "<pre><code class=\"language-rust\">fn cut_short_for_lack_of_time() {}\n</code></pre>"
                .into(),
        );
        let messages = vec![
            message("a1", "alice", "Some code:"),
            code,
            message("b1", "bob", "Thanks"),
        ];
        let mut world = World::new(messages);
        let n = world.len();
        // Its time was spent before its block was looked at (by another block, say).
        let late = crate::highlight::OutOfTime::start();
        world.step("out of time");
        let plain = rows_of(&world, "k1");
        assert_eq!(world.step("still out of time"), n - 1);
        assert_eq!(rows_of(&world, "k1"), plain);
        drop(late);
        assert_eq!(world.step("time enough"), n - 1);
        assert_ne!(rows_of(&world, "k1"), plain, "coloured now");
        assert_eq!(world.step("again"), n);
    }

    #[test]
    fn the_unread_line_moves_with_the_messages_next_to_it() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();
        let at = world.index("line");
        let line = world.room.messages.remove(at);
        world.room.messages.insert(3, line);
        // The line itself is kept; the messages after its old and its new place are
        // not (the first after it gets its sender's header).
        assert_eq!(world.step("the line moves"), n - 2);
        let at = world.index("line");
        world.room.messages.remove(at);
        world.step("the line goes");
    }

    #[test]
    fn the_width_the_theme_and_the_settings_lay_out_everything() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();
        world.width = 61;
        assert_eq!(world.step("wider"), 0);
        assert_eq!(world.step("the same"), n);
        world.theme = Theme::builtin("nord").unwrap();
        world.theme_rev += 1;
        assert_eq!(world.step("a theme"), 0);
        world.read_markers = false;
        assert_eq!(world.step("read markers off"), 0);
        world.read_markers = true;
        assert_eq!(world.step("read markers on"), 0);
        world.room.kind = RoomKind::Person;
        assert_eq!(world.step("a chat with one person"), 0);
        // Previews off, and someone ignored: the messages they are in.
        world.preview_arrives(LINK, "The plan");
        world.step("a preview");
        world.mode = LinkPreviews::Unencrypted;
        world.room.encrypted = true;
        world.step("previews off here");
        world.ignored.insert("@carol:example.org".into());
        world.step("carol ignored");
        world.revealed = Some(world.index("b2"));
        assert_eq!(world.step("a spoiler shown"), n - 1);
    }

    #[test]
    fn notes_lay_out_their_messages() {
        let mut world = World::new(sample());
        world.step("first");
        let n = world.len();
        world
            .notes
            .set("$a1", NoteKind::Translation, NoteState::Running);
        assert_eq!(world.step("translating"), n - 1);
        world.notes.set(
            "$a1",
            NoteKind::Translation,
            NoteState::Done("Guten Morgen".into()),
        );
        assert_eq!(world.step("translated"), n - 1);
        world
            .notes
            .set("$a5", NoteKind::Transcript, NoteState::Done("hello".into()));
        assert_eq!(world.step("transcribed"), n - 1);
        world.notes.toggle_hidden("$a1", NoteKind::Translation);
        assert_eq!(world.step("hidden"), n - 1);
        world.notes.toggle_hidden("$a1", NoteKind::Translation);
        world.step("shown");
        // Another language: what was translated is not current any more.
        world.stale_translations = true;
        assert_eq!(world.step("another language"), n - 1);
    }

    #[test]
    fn numbers_are_written_in_as_few_bytes_as_they_need_and_stay_apart() {
        let keys = RandomState::new();
        let bytes = |write: &dyn Fn(&mut Feed)| {
            let mut feed = Feed {
                keys: &keys,
                bytes: Vec::new(),
            };
            write(&mut feed);
            feed.bytes
        };
        assert_eq!(bytes(&|f| f.write_usize(0)), [0]);
        assert_eq!(bytes(&|f| f.write_u32(127)), [0x7f]);
        assert_eq!(bytes(&|f| f.write_u64(128)), [0x80, 0x01]);
        assert_eq!(bytes(&|f| f.write_u64(300)), [0xac, 0x02]);
        assert_eq!(bytes(&|f| f.write_u64(u64::MAX)).len(), 10);
        assert_eq!(bytes(&|f| f.write_isize(-1)).len(), 10);
        // What the standard library writes for strings and lists keeps them apart.
        let hashed = |value: &dyn Fn(&mut Feed)| bytes(value);
        assert_ne!(
            hashed(&|f| ["ab", ""].hash(f)),
            hashed(&|f| ["a", "b"].hash(f))
        );
        assert_ne!(
            hashed(&|f| (Some(1u32), 2u64).hash(f)),
            hashed(&|f| (Some(258u32), None::<u64>).hash(f))
        );
        // Same bytes, same hash; the keys are those of the memo.
        let mut one = Feed {
            keys: &keys,
            bytes: Vec::new(),
        };
        "hello".hash(&mut one);
        assert_eq!(one.finish(), keys.hash_one(one.bytes.as_slice()));
    }

    /// A small deterministic source of choices (xorshift).
    struct Dice(u64);

    impl Dice {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n.max(1) as u64) as usize
        }

        fn pick<'a, T>(&mut self, of: &'a [T]) -> &'a T {
            &of[self.below(of.len())]
        }
    }

    /// Makes one change of the kind the room and the app see, at random. What it was.
    fn change(world: &mut World, dice: &mut Dice, made: &mut usize) -> String {
        const SENDERS: [&str; 5] = ["alice", "bob", "carol", "you", "zed"];
        const EMOJI: [&str; 3] = [BLOB, PARTY, "mxc://example.org/wave"];
        *made += 1;
        let made = *made;
        let len = world.len();
        let i = dice.below(len);
        let what = dice.below(38);
        let message = &mut world.room.messages[i];
        let id = message.id.clone();
        match what {
            0 => {
                message.body = format!("edited {made}");
                message.html = None;
                message.edited = true;
            }
            1 => message
                .reactions
                .push(reaction(dice.pick(&["👍", "🎉", PARTY]), 1)),
            2 => {
                if let Some(reaction) = message.reactions.last_mut() {
                    reaction.count += 1;
                    reaction.own = !reaction.own;
                }
            }
            3 => {
                message.reactions.pop();
            }
            4 => {
                for message in &mut world.room.messages {
                    message.read_by = ReadBy::default();
                }
                // Some of them, as many as before or not.
                let who: Vec<&str> = SENDERS
                    .iter()
                    .copied()
                    .filter(|_| dice.below(2) == 0)
                    .collect();
                world.room.messages[i].read_by = readers(&who);
            }
            5 => {
                message.kind = MsgKind::Redacted;
                message.body = "Message deleted".into();
                message.html = None;
            }
            6 => {
                let thread = message.thread.get_or_insert_with(ThreadSummary::default);
                thread.replies += 1;
                thread.unread = !thread.unread;
                thread.latest = Some(("bob".into(), format!("reply {made}")));
            }
            7 => {
                message.state =
                    *dice.pick(&[SendState::Sent, SendState::Sending, SendState::Failed]);
                message.upload = (dice.below(2) == 0).then_some((made as u64, 100));
            }
            8 => {
                message.shield = *dice.pick(&[
                    None,
                    Some(Shield::SenderMismatch),
                    Some(Shield::UnverifiedDevice),
                ]);
            }
            9 => {
                message.pinned = !message.pinned;
                message.saved = dice.below(2) == 0;
            }
            10 => message.day = (*dice.pick(&["Monday", "Tuesday", "Wednesday"])).to_owned(),
            // The messages of a day are of another one: the divider before the first of
            // them says so.
            35 => {
                let day = world.room.messages[i].day.clone();
                let messages = &mut world.room.messages;
                let start = messages[..i]
                    .iter()
                    .rposition(|m| m.day != day)
                    .map_or(0, |k| k + 1);
                let end = messages[i..]
                    .iter()
                    .position(|m| m.day != day)
                    .map_or(len, |k| i + k);
                for message in &mut messages[start..end] {
                    message.day = format!("Day {made}");
                }
            }
            11 => {
                message.sender = (*dice.pick(&SENDERS)).to_owned();
                message.sender_id = Some(format!("@{}:example.org", message.sender));
            }
            12 if len > 4 => {
                world.room.messages.remove(i);
            }
            13 => {
                let sender = *dice.pick(&SENDERS);
                let mut new = message_like(world, dice, made, sender);
                new.day = world.room.messages[i].day.clone();
                world.room.messages.insert(i, new);
            }
            14 => {
                let sender = *dice.pick(&SENDERS);
                let new = message_like(world, dice, made, sender);
                world.room.messages.push(new);
            }
            15 => {
                let older: Vec<Message> = (0..dice.below(3) + 1)
                    .map(|k| message_like(world, dice, made * 10 + k, "olga"))
                    .collect();
                world.room.messages.splice(0..0, older);
            }
            16 => {
                world
                    .room
                    .messages
                    .retain(|m| m.kind != MsgKind::ReadMarker);
                let at = dice.below(world.len() + 1);
                world.room.messages.insert(at, line());
            }
            17 => {
                if let Some(poll) = &mut message.poll {
                    poll.answers[0].votes += 1;
                    poll.total_voters += 1;
                    poll.ended = dice.below(3) == 0;
                }
            }
            18 if i + 1 < len => world.room.messages.swap(i, i + 1),
            19 => {
                if let Some(image) = &message.image {
                    let key = image.key.clone();
                    let result = match dice.below(3) {
                        0 => Err("broken"),
                        _ => Ok((100 + dice.below(500) as u32, 50 + dice.below(400) as u32)),
                    };
                    world.picture_arrives(&key, result);
                }
            }
            20 => {
                let url = format!("https://example.org/{}", dice.below(made + 1));
                world.preview_arrives(&url, &format!("Page {made}"));
                world.preview_arrives(LINK, "The plan");
            }
            21 => world.emoji_arrives(dice.pick(&EMOJI)),
            22 => world.width = *dice.pick(&[24, 40, 59, 60, 61, 80]),
            23 => {
                let names = Theme::builtin_names();
                world.theme = Theme::builtin(dice.pick(&names)).unwrap();
                world.theme_rev += 1;
            }
            24 => world.read_markers = !world.read_markers,
            25 => {
                world.room.kind = match world.room.kind {
                    RoomKind::Person => RoomKind::Room,
                    _ => RoomKind::Person,
                };
            }
            26 => {
                world.mode = *dice.pick(&LinkPreviews::ALL);
                world.room.encrypted = dice.below(2) == 0;
            }
            27 => {
                let who = format!("@{}:example.org", dice.pick(&SENDERS));
                if !world.ignored.remove(&who) {
                    world.ignored.insert(who);
                }
            }
            28 => {
                if world.catalog.packs.is_empty() {
                    world.catalog.packs.push(party_pack());
                } else {
                    world.catalog.packs.clear();
                }
            }
            29 => {
                let event = format!("${id}");
                let kind = *dice.pick(&NoteKind::ALL);
                match dice.below(4) {
                    0 => world.notes.set(&event, kind, NoteState::Running),
                    1 => world
                        .notes
                        .set(&event, kind, NoteState::Done(format!("note {made}"))),
                    2 => {
                        world.notes.toggle_hidden(&event, kind);
                    }
                    _ => world.notes.remove(&event, kind),
                }
            }
            30 => world.stale_translations = !world.stale_translations,
            31 => {
                world.revealed = match dice.below(3) {
                    0 => None,
                    _ => Some(i),
                }
            }
            32 => {
                world.images = ImageStore::new(match dice.below(3) {
                    0 => kitty(),
                    1 => crate::images::Graphics::halfblocks(),
                    _ => crate::images::Graphics::off(),
                });
            }
            33 => message.kind = *dice.pick(&[MsgKind::Text, MsgKind::Emote, MsgKind::System]),
            34 => {
                message.reply_to = Some(("carol".into(), format!("quoted {made}")));
                message.reply_event = (dice.below(2) == 0).then(|| "$c1".to_owned());
                message.reply_sender =
                    (dice.below(2) == 0).then(|| "@carol:example.org".to_owned());
            }
            // Someone of the same name on another server, or the same person again; and
            // whether the SDK finds that name to be someone else's too.
            36 => {
                let server = dice.pick(&["example.org", "elsewhere.org"]);
                message.sender_id = Some(format!("@{}:{server}", message.sender));
                message.sender_ambiguous = dice.below(2) == 0;
            }
            // Nothing: the same messages again.
            _ => return "nothing".to_owned(),
        }
        format!("change {what} at {i}")
    }

    /// A new message of a kind picked at random.
    fn message_like(world: &World, dice: &mut Dice, made: usize, sender: &str) -> Message {
        let mut new = message(&format!("n{made}"), sender, &format!("message {made}"));
        new.day = world
            .room
            .messages
            .last()
            .map_or("Monday".to_owned(), |m| m.day.clone());
        match dice.below(6) {
            0 => new.body = format!("look at https://example.org/{made}"),
            1 => {
                new.html = Some(format!(
                    "an <img data-mx-emoticon src=\"{}\" alt=\":e:\"> emoji",
                    dice.pick(&[BLOB, PARTY, "mxc://example.org/wave"])
                ));
            }
            2 => new.image = Some(remote_image(&format!("mxc://example.org/p{made}"), None)),
            3 => {
                new.html = Some(format!("a <span data-mx-spoiler>secret {made}</span>"));
            }
            4 => new.reactions = vec![reaction(PARTY, 1), reaction("👍", 2)],
            _ => {}
        }
        new
    }

    /// The main proof: after every change of a long run of them, of every kind the
    /// rows depend on, the rows kept and laid out are those laid out from scratch.
    #[test]
    fn kept_rows_are_always_those_laid_out_from_scratch() {
        let mut world = World::new(sample());
        let mut dice = Dice(0x9e37_79b9_7f4a_7c15);
        let mut made = 0;
        world.step("first");
        let (mut kept, mut messages, mut partly) = (0, 0, 0);
        for step in 0..700 {
            let what = change(&mut world, &mut dice, &mut made);
            let now = world.step(&format!("step {step}: {what}"));
            kept += now;
            messages += world.len();
            partly += usize::from(now > 0 && now < world.len());
        }
        // Most messages kept their rows, and many changes laid out some of them anew.
        assert!(kept * 2 > messages, "{kept} of {messages}");
        assert!(partly > 200, "{partly}");
    }
}
