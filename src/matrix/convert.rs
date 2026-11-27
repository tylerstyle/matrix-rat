//! Turning SDK timeline items into what the UI shows.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, BuildHasherDefault, Hasher, RandomState};
use std::sync::Arc;

use chrono::{DateTime, Local, TimeZone};
use matrix_sdk::ruma::events::room::message::{
    AudioMessageEventContent, LocationMessageEventContent, MessageType,
};
use matrix_sdk::ruma::events::room::{ImageInfo as RumaImageInfo, MediaSource};
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedUserId, UInt, UserId};
use matrix_sdk_base::crypto::types::events::UtdCause;
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    EncryptedMessage, EventSendState, EventTimelineItem, MembershipChange, MsgLikeKind, Profile,
    TimelineDetails, TimelineEventItemId, TimelineItem, TimelineItemContent, VirtualTimelineItem,
};

use crate::model::{
    Attachment, AudioMeta, DeletedBy, Deletion, ImageInfo, ImageSource, ItemRef, MediaKind,
    MediaRef, Message, MsgKind, Reaction, SendState, Shield, UtdWhy, disambiguated,
};
use crate::text::{sanitize, sanitize_line};

/// The messages of `items`, every one made afresh: what [`Reuse::convert`] gives too,
/// which the worker uses. For tests and measurements.
#[cfg(test)]
pub fn convert(
    items: &Vector<Arc<TimelineItem>>,
    own: &UserId,
    ignored: &HashSet<OwnedUserId>,
) -> Vec<Message> {
    convert_at(items, own, ignored, &Local::now())
}

/// [`convert`] as it is at `now`.
#[cfg(test)]
pub(super) fn convert_at(
    items: &Vector<Arc<TimelineItem>>,
    own: &UserId,
    ignored: &HashSet<OwnedUserId>,
    now: &DateTime<Local>,
) -> Vec<Message> {
    let last = items.len().saturating_sub(1);
    // Room for every item, made once: collected from a `filter_map`, the list grew by
    // doubling, and the interface keeps this very list, with up to as many unused
    // messages again (784 bytes each) for every room it has shown.
    let mut messages = Vec::with_capacity(items.len());
    messages.extend(
        items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| match item.as_event() {
                Some(event) => event_message(item.unique_id().0.clone(), event, own, ignored, now),
                None => read_marker(item, index == last),
            }),
    );
    messages
}

/// The line of a virtual item, if it has one: the read marker, kept even when every
/// item after it was hidden or ignored, so the UI knows no visible unread messages
/// follow it; but not as the very last item. Day separators are derived from the
/// messages' own dates.
fn read_marker(item: &TimelineItem, last: bool) -> Option<Message> {
    (matches!(item.as_virtual()?, VirtualTimelineItem::ReadMarker) && !last).then(|| Message {
        id: item.unique_id().0.clone(),
        kind: MsgKind::ReadMarker,
        ..Message::default()
    })
}

/// The messages of a timeline that changes: what was made of each event item is kept
/// and only what is new or changed is made again. A change of the open room is mostly
/// one item, and making every message afresh took 9 ms in a room of 5000.
///
/// What a message is made from:
///
/// - its item. The SDK never changes one: an edit, a reaction, a read receipt, a
///   decryption, a send state, the details of the message replied to, a sender's
///   profile or a thread's summary each put a new `Arc<TimelineItem>` in its place
///   (matrix-sdk-ui 0.19.1 and 0.20.0: `TimelineItem::with_kind`,
///   `ObservableItems::replace`), and nothing in an item changes behind a shared
///   reference. What was made is found by the address of the item and holds the item,
///   so that no other item can be put at that address while it is kept: the same
///   address is the same item, unchanged.
/// - whose account it is, whom the user ignores (hidden senders, reaction counts,
///   votes, quoted replies), and today's date and the local time's offset ("Today",
///   "Yesterday", the time). These are the same for every message; when one of them
///   changes, everything is made again.
/// - for a virtual item, whether it is the last one. Virtual items are not kept: a
///   read marker costs nothing to make.
///
/// What comes after (own name, read receipts, repeated events, threads) works on the
/// whole list and is done again on every change.
///
/// It keeps one message per event item there is (`None` for one not shown): as much
/// again as the list the interface is sent. Those of items that are gone are let go.
#[derive(Default)]
pub(super) struct Reuse {
    made: HashMap<usize, Made, BuildHasherDefault<Address>>,
    /// What everything in `made` was made with, besides its item.
    with: Option<With>,
    /// Counts the passes; an entry not seen in the last one is of an item that is gone.
    pass: u64,
    /// How many messages the last pass made and how many it used again.
    #[cfg(test)]
    pub(super) counts: (usize, usize),
}

/// What [`Reuse`] keeps for one item.
struct Made {
    /// Kept so that its address, the key, stays its own.
    item: Arc<TimelineItem>,
    /// `None` for an item that is not shown. Boxed: a hash table has room for more
    /// entries than it holds, and a message is large.
    message: Option<Box<Message>>,
    pass: u64,
}

/// What every message [`Reuse`] keeps was made with, besides its item.
struct With {
    own: OwnedUserId,
    ignored: HashSet<OwnedUserId>,
    day: chrono::NaiveDate,
    /// Seconds east of UTC.
    offset: i32,
}

impl Reuse {
    /// Lets go of everything kept: the timeline was cleared or reset.
    pub(super) fn forget(&mut self) {
        self.made = HashMap::default();
    }

    /// The messages of `items`, as if every one was made afresh; made again only for
    /// the items that are new or changed since the last call.
    pub(super) fn convert(
        &mut self,
        items: &Vector<Arc<TimelineItem>>,
        own: &UserId,
        ignored: &HashSet<OwnedUserId>,
    ) -> Vec<Message> {
        self.convert_at(items, own, ignored, &Local::now())
    }

    /// [`Reuse::convert`] as it is at `now`.
    pub(super) fn convert_at(
        &mut self,
        items: &Vector<Arc<TimelineItem>>,
        own: &UserId,
        ignored: &HashSet<OwnedUserId>,
        now: &DateTime<Local>,
    ) -> Vec<Message> {
        let same = self.with.as_ref().is_some_and(|with| {
            *with.own == *own
                && with.ignored == *ignored
                && with.day == now.date_naive()
                && with.offset == now.offset().local_minus_utc()
        });
        if !same {
            self.made.clear();
            self.with = Some(With {
                own: own.to_owned(),
                ignored: ignored.clone(),
                day: now.date_naive(),
                offset: now.offset().local_minus_utc(),
            });
        }
        self.pass = self.pass.wrapping_add(1);
        let pass = self.pass;
        #[cfg(test)]
        {
            self.counts = (0, 0);
        }
        let last = items.len().saturating_sub(1);
        // Room for every item, made once: the interface keeps this very list.
        let mut messages = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let Some(event) = item.as_event() else {
                messages.extend(read_marker(item, index == last));
                continue;
            };
            let made = match self.made.entry(Arc::as_ptr(item) as usize) {
                Entry::Occupied(found) => {
                    #[cfg(test)]
                    {
                        self.counts.1 += 1;
                    }
                    found.into_mut()
                }
                Entry::Vacant(free) => {
                    #[cfg(test)]
                    {
                        self.counts.0 += 1;
                    }
                    let message =
                        event_message(item.unique_id().0.clone(), event, own, ignored, now);
                    free.insert(Made {
                        item: item.clone(),
                        message: message.map(Box::new),
                        pass,
                    })
                }
            };
            debug_assert!(Arc::ptr_eq(&made.item, item));
            made.pass = pass;
            if let Some(message) = &made.message {
                messages.push(Message::clone(message));
            }
        }
        self.made.retain(|_, made| made.pass == pass);
        messages
    }
}

/// Hashes the address of an item. Its low bits are the same for every item (they are
/// aligned), so it is spread over all bits.
#[derive(Default)]
struct Address(u64);

impl Hasher for Address {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3);
        }
    }

    fn write_usize(&mut self, address: usize) {
        let spread = (address as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        self.0 = spread ^ (spread >> 29);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Why a message could not be decrypted, from what the SDK worked out.
pub(super) fn utd_why(encrypted: &EncryptedMessage) -> UtdWhy {
    let EncryptedMessage::MegolmV1AesSha2 { cause, .. } = encrypted else {
        return UtdWhy::Unsupported;
    };
    match cause {
        UtdCause::Unknown => UtdWhy::NoKey,
        UtdCause::SentBeforeWeJoined => UtdWhy::SentBeforeJoin,
        UtdCause::VerificationViolation => UtdWhy::SenderIdentityChanged,
        UtdCause::UnsignedDevice => UtdWhy::SenderDeviceUnsigned,
        UtdCause::UnknownDevice => UtdWhy::SenderDeviceUnknown,
        UtdCause::HistoricalMessageAndBackupIsDisabled => UtdWhy::HistoryNoBackup,
        UtdCause::HistoricalMessageAndDeviceIsUnverified => UtdWhy::HistoryNeedsVerification,
        UtdCause::WithheldForUnverifiedOrInsecureDevice => UtdWhy::WithheldUnverified,
        UtdCause::WithheldBySender => UtdWhy::Withheld,
        UtdCause::Malformed => UtdWhy::Malformed,
    }
}

/// Pass-through hasher for digests already computed with [`RandomState`].
#[derive(Default)]
struct Prehashed(u64);

impl Hasher for Prehashed {
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.wrapping_mul(0x1000_0000_01b3) ^ u64::from(byte);
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// Reusable scratch table for [`EventDedup::dedup`].
///
/// Kept per open timeline task so updates with no repeated events do a single linear
/// scan with zero heap allocations after warm-up.
#[derive(Default)]
pub(super) struct EventDedup {
    seen: HashSet<u64, BuildHasherDefault<Prehashed>>,
    hasher: RandomState,
}

impl EventDedup {
    /// Removes duplicate event IDs from `messages` in place: each event is kept once,
    /// at its first copy's position (preserving that copy's stable `Message.id`), with
    /// the last copy's content (edits, reactions, redactions, decryptions, receipts).
    ///
    /// Works around a `matrix-sdk-ui 0.19.1` race in `TimelineBuilder::build()` where a
    /// live sync batch can be replayed on top of `event_cache.events()`, leaving a block
    /// of events in the timeline twice while later updates target the trailing copy.
    /// Fixed in 0.20.0 (matrix-rust-sdk PR #7284, and #7468 for updates sent out of
    /// order); kept as a safety net, since a timeline that falls behind its updates
    /// (`RecvError::Lagged`) reloads its events but keeps the old receiver and can still
    /// apply them twice (matrix-rust-sdk issue #7533, not named as fixed in 0.20.0).
    pub(super) fn dedup(&mut self, messages: &mut Vec<Message>) {
        if messages.len() < 2 {
            return;
        }
        self.seen.clear();
        self.seen.reserve(messages.len());
        let mut repeated = false;
        for message in messages.iter() {
            if let Some(id) = event_id(message)
                && !self.seen.insert(self.hasher.hash_one(id))
            {
                repeated = true;
                break;
            }
        }
        if !repeated {
            return;
        }
        dedup_with_latest(messages);
    }
}

fn event_id(message: &Message) -> Option<&str> {
    match message.item.as_ref()? {
        ItemRef::Event(id) => Some(id.as_str()),
        ItemRef::Transaction(_) => None,
    }
}

/// Exact deduplication pass run only when [`EventDedup::dedup`] spots a repeat (or a
/// hash collision). Keeps the first slot and its `Message.id`, takes the last copy's
/// content, leaves non-event items untouched, and keeps any `ReadMarker` right after
/// its preceding event's surviving copy.
fn dedup_with_latest(messages: &mut Vec<Message>) {
    let n = messages.len();
    let (dest, rm_relocate) = {
        let mut first: HashMap<&str, usize> = HashMap::with_capacity(n);
        let mut dest: Vec<usize> = Vec::with_capacity(n);
        let mut any_dup = false;
        let mut rm_pos = None;
        for (i, message) in messages.iter().enumerate() {
            if rm_pos.is_none() && message.kind == MsgKind::ReadMarker {
                rm_pos = Some(i);
            }
            if let Some(id) = event_id(message) {
                match first.entry(id) {
                    std::collections::hash_map::Entry::Vacant(slot) => {
                        slot.insert(i);
                        dest.push(i);
                    }
                    std::collections::hash_map::Entry::Occupied(slot) => {
                        dest.push(*slot.get());
                        any_dup = true;
                    }
                }
            } else {
                dest.push(i);
            }
        }
        if !any_dup {
            // Spurious hash collision in the fast-path check; leave the list untouched.
            return;
        }
        // `ReadMarker` sits immediately after the last read event. If the SDK placed it
        // after a trailing duplicate (its `update_read_marker` searches from the back),
        // move it right after that event's surviving first copy.
        let rm_relocate = rm_pos.and_then(|rm| {
            let prev_event = messages[..rm]
                .iter()
                .rposition(|message| event_id(message).is_some())?;
            let anchor = dest[prev_event];
            (anchor < prev_event).then_some((rm, anchor))
        });
        (dest, rm_relocate)
    };

    // Move each later copy's content into its first copy's slot while keeping the first
    // copy's `id` so UI anchors (selection, scroll, edit/reply, playback) do not jump.
    for (i, &target) in dest.iter().enumerate() {
        if target < i {
            let (head, tail) = messages.split_at_mut(i);
            let first = &mut head[target];
            let later = &mut tail[0];
            // The SDK decrypts whichever copy it finds by position (keys imported from a
            // file, say), often the first: see `takes_over`.
            if !takes_over(
                first.kind == MsgKind::Undecryptable,
                later.kind == MsgKind::Undecryptable,
                true,
            ) {
                continue;
            }
            std::mem::swap(first, later);
            std::mem::swap(&mut first.id, &mut later.id);
        }
    }

    // Compact out superseded slots and track where the `ReadMarker` and its anchor land.
    let mut new_rm = None;
    let mut new_anchor = None;
    let mut write = 0;
    for (read, &target) in dest.iter().enumerate() {
        if target == read {
            if let Some((rm_pos, anchor)) = rm_relocate {
                if read == rm_pos {
                    new_rm = Some(write);
                }
                if read == anchor {
                    new_anchor = Some(write);
                }
            }
            if write != read {
                messages.swap(write, read);
            }
            write += 1;
        }
    }
    messages.truncate(write);

    if let (Some(rm_at), Some(anchor_at)) = (new_rm, new_anchor) {
        let insert_at = anchor_at + 1;
        if insert_at < rm_at {
            messages[insert_at..=rm_at].rotate_right(1);
        }
    }
}

/// Of two copies of one event that the SDK's timeline holds (see [`EventDedup`]),
/// whether `other` is the one to show in place of `shown`; `otherwise` says which it
/// would be were both locked or both readable. Decryption goes one way only, so a copy
/// still locked never stands in for one that was decrypted, and one decrypted always
/// does for one still locked.
pub(super) fn takes_over(shown_locked: bool, other_locked: bool, otherwise: bool) -> bool {
    if shown_locked == other_locked {
        otherwise
    } else {
        shown_locked
    }
}

/// Applies the room's current synced own display name to already-loaded own messages so
/// non-ambiguous display name changes do not wait for old SDK timeline items to refresh.
pub(super) fn apply_own_display_name(messages: &mut [Message], own_name: Option<&str>) {
    let Some(name) = own_name else {
        return;
    };
    if crate::text::is_blank(name) {
        return;
    }
    for msg in messages {
        if msg.own && !matches!(msg.kind, MsgKind::System | MsgKind::ReadMarker) {
            msg.sender = name.to_owned();
        }
    }
}

/// One line saying what a message is, for the room list.
pub(super) fn preview(content: &TimelineItemContent) -> String {
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "foreign matrix_sdk_ui TimelineItemContent, MsgLikeKind, and ruma MessageType"
    )]
    let text = match content {
        TimelineItemContent::MsgLike(content) => match &content.kind {
            MsgLikeKind::Message(message) => match message.msgtype() {
                MessageType::Image(_) => "sent a picture".to_owned(),
                MessageType::Video(_) => "sent a video".to_owned(),
                MessageType::Audio(audio) => {
                    if audio.voice.is_some() {
                        "sent a voice message".to_owned()
                    } else {
                        "sent an audio message".to_owned()
                    }
                }
                MessageType::Location(loc) => format!("📍 {}", location_label(loc)),
                MessageType::File(file) => format!("📎 {}", file.filename()),
                other => other.body().to_owned(),
            },
            MsgLikeKind::Sticker(_) => "sent a sticker".to_owned(),
            MsgLikeKind::Poll(poll) => {
                let q = sanitize_line(&poll.results().question);
                let q = q.trim();
                if q.is_empty() {
                    "started a poll".to_owned()
                } else {
                    format!("📊 {q}")
                }
            }
            MsgLikeKind::Redacted => "deleted a message".to_owned(),
            MsgLikeKind::UnableToDecrypt(_) => "encrypted message".to_owned(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    // The first line with something on it, without a quoted reply above it.
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('>'))
        .unwrap_or_default();
    crate::text::truncate(&sanitize_line(line), 160)
}

/// The name of `user`: their display name, or the first part of their ID where they
/// have none (which the SDK counts as their name too, when it looks for others of the
/// same name).
pub(super) fn display_name(profile: &TimelineDetails<Profile>, user: &UserId) -> String {
    if let TimelineDetails::Ready(Profile {
        display_name: Some(name),
        ..
    }) = profile
        && !crate::text::is_blank(&sanitize_line(name))
    {
        sanitize_line(name)
    } else {
        user.localpart().to_owned()
    }
}

/// Whether someone else in the room goes by the same name, as far as the SDK knows (in
/// a room whose members are not all loaded yet it may not know).
pub(super) fn ambiguous(profile: &TimelineDetails<Profile>) -> bool {
    matches!(
        profile,
        TimelineDetails::Ready(Profile {
            display_name_ambiguous: true,
            ..
        })
    )
}

/// The name of `user` as shown: with their ID after it where someone else in the room
/// goes by the same name.
pub(super) fn shown_name(profile: &TimelineDetails<Profile>, user: &UserId) -> String {
    let name = display_name(profile, user);
    if ambiguous(profile) {
        disambiguated(&name, user.as_str())
    } else {
        name
    }
}

pub(super) fn local_time(timestamp: MilliSecondsSinceUnixEpoch) -> Option<DateTime<Local>> {
    let millis = i64::try_from(u64::from(timestamp.get())).ok()?;
    Local.timestamp_millis_opt(millis).single()
}

/// "Today", "Yesterday", or a date.
pub fn day_label(when: &DateTime<Local>, now: &DateTime<Local>) -> String {
    let days = (now.date_naive() - when.date_naive()).num_days();
    match days {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ => when.format("%a %-d %b %Y").to_string(),
    }
}

/// `None` for events that are not worth a line in a chat view.
pub(super) fn event_message(
    id: String,
    event: &EventTimelineItem,
    own: &UserId,
    ignored: &HashSet<OwnedUserId>,
    now: &DateTime<Local>,
) -> Option<Message> {
    if !event.is_own() && ignored.contains(event.sender()) {
        return None;
    }
    let sender = display_name(event.sender_profile(), event.sender());
    let sender_ambiguous = ambiguous(event.sender_profile());
    // In a line of its own ("… joined") the name stands alone: told apart there.
    let shown = shown_name(event.sender_profile(), event.sender());
    let when = local_time(event.timestamp());
    let mut message = Message {
        shield: shield(event),
        id,
        item: Some(match event.identifier() {
            TimelineEventItemId::EventId(id) => ItemRef::Event(id.to_string()),
            TimelineEventItemId::TransactionId(id) => ItemRef::Transaction(id.to_string()),
        }),
        state: match event.send_state() {
            Some(EventSendState::NotSentYet { .. }) => SendState::Sending,
            Some(EventSendState::SendingFailed { .. }) => SendState::Failed,
            _ => SendState::Sent,
        },
        time: when
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_default(),
        day: when.map(|t| day_label(&t, now)).unwrap_or_default(),
        sent_at: Some(u64::from(event.timestamp().get())),
        encrypted: event.encryption_info().is_some() || event.content().is_unable_to_decrypt(),
        own: event.is_own(),
        sender,
        sender_id: Some(event.sender().to_string()),
        sender_ambiguous,
        ..Message::default()
    };
    match event.send_state() {
        Some(EventSendState::NotSentYet {
            progress: Some(upload),
        }) => {
            let progress = upload.progress;
            message.upload = Some((progress.current as u64, progress.total as u64));
        }
        Some(EventSendState::SendingFailed { error, .. }) => {
            message.send_error = Some(super::upload::failure_reason(error));
        }
        _ => {}
    }
    // Our changes to it that could not be sent: they wait for Enter or Del, and hold up
    // what we send after them in the room (see `prepare_messages`).
    if let Some(EventSendState::SendingFailed { error, .. }) = event.edit_send_state() {
        message.edit_error = Some(super::upload::failure_reason(error));
    }
    if let Some(EventSendState::SendingFailed { error, .. }) = event.redaction_send_state() {
        message.delete_error = Some(super::upload::failure_reason(error));
    }

    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "foreign matrix_sdk_ui TimelineItemContent, TimelineDetails, MsgLikeKind, and ruma MessageType"
    )]
    match event.content() {
        TimelineItemContent::MsgLike(content) => {
            message.thread = super::threads::summary(content, ignored);
            // Since matrix-sdk-ui 0.20 any event can carry reactions; they are shown
            // under messages only, as before, and those on other events are left out.
            message.reactions = event
                .reactions()
                .iter()
                .filter_map(|(key, senders)| {
                    let count = senders
                        .keys()
                        .filter(|user| !ignored.contains(*user))
                        .count() as u32;
                    let ours = senders.get(own);
                    (count > 0).then(|| Reaction {
                        key: key.clone(),
                        count,
                        own: ours.is_some(),
                        failed: ours.is_some_and(|info| {
                            matches!(info.send_state, Some(EventSendState::SendingFailed { .. }))
                        }),
                    })
                })
                .collect();
            message.reply_to = content
                .in_reply_to
                .as_ref()
                .map(|reply| match &reply.event {
                    TimelineDetails::Ready(original) if ignored.contains(&original.sender) => {
                        (String::new(), "message from ignored user".to_owned())
                    }
                    TimelineDetails::Ready(original) => (
                        shown_name(&original.sender_profile, &original.sender),
                        original
                            .content
                            .as_message()
                            .map(|m| m.body().to_owned())
                            .unwrap_or_else(|| "a message".to_owned()),
                    ),
                    _ => (String::new(), "an earlier message".to_owned()),
                });
            message.reply_sender =
                content
                    .in_reply_to
                    .as_ref()
                    .and_then(|reply| match &reply.event {
                        TimelineDetails::Ready(original) if !ignored.contains(&original.sender) => {
                            Some(original.sender.to_string())
                        }
                        _ => None,
                    });
            message.reply_event = content
                .in_reply_to
                .as_ref()
                .map(|reply| reply.event_id.to_string());
            match &content.kind {
                MsgLikeKind::Message(msg) => {
                    message.edited = msg.is_edited();
                    message.mentions = msg.mentions().cloned();
                    message.body = match msg.msgtype() {
                        MessageType::Emote(c) => {
                            message.kind = MsgKind::Emote;
                            message.html = c.formatted.as_ref().map(|f| f.body.clone());
                            c.body.clone()
                        }
                        MessageType::Text(c) => {
                            message.html = c.formatted.as_ref().map(|f| f.body.clone());
                            c.body.clone()
                        }
                        MessageType::Notice(c) => {
                            message.html = c.formatted.as_ref().map(|f| f.body.clone());
                            c.body.clone()
                        }
                        MessageType::Image(c) => {
                            let image = image_info(
                                c.source.clone(),
                                c.filename(),
                                c.caption(),
                                c.info.as_deref(),
                            );
                            let text = image_fallback(&image);
                            message.image = Some(image);
                            text
                        }
                        MessageType::File(c) => {
                            let size = c.info.as_ref().and_then(|info| info.size);
                            attach_media(
                                &mut message,
                                MediaKind::File,
                                "📎",
                                &c.source,
                                c.filename(),
                                c.caption(),
                                size,
                                None,
                                None,
                            )
                        }
                        MessageType::Video(c) => {
                            let info = c.info.as_deref();
                            let summary = attach_media(
                                &mut message,
                                MediaKind::Video,
                                "🎞 ",
                                &c.source,
                                c.filename(),
                                c.caption(),
                                info.and_then(|i| i.size),
                                info.and_then(|i| i.duration),
                                None,
                            );
                            if let Some(thumb_src) = info.and_then(|i| i.thumbnail_source.clone()) {
                                let thumb_info = info.and_then(|i| i.thumbnail_info.as_deref());
                                let dimensions = thumb_info
                                    .and_then(|ti| parse_dimensions(ti.width, ti.height))
                                    .or_else(|| {
                                        info.and_then(|i| parse_dimensions(i.width, i.height))
                                    });
                                let size = thumb_info.and_then(|ti| ti.size).map(u64::from);
                                let name = message
                                    .file
                                    .as_ref()
                                    .map_or_else(|| "video".to_owned(), |f| f.name.clone());
                                message.image = Some(ImageInfo {
                                    key: sanitize_line(&media_source_key(&thumb_src)),
                                    source: ImageSource::Matrix(Box::new(MediaRef {
                                        full: thumb_src,
                                        thumbnail: None,
                                    })),
                                    name,
                                    caption: None,
                                    dimensions,
                                    size,
                                    sticker: false,
                                });
                            }
                            summary
                        }
                        MessageType::Audio(c) => {
                            let is_voice = c.voice.is_some();
                            let (kind, icon) = if is_voice {
                                (MediaKind::Voice, "🎤")
                            } else {
                                (MediaKind::Audio, "🔊")
                            };
                            let meta = audio_meta(c);
                            // The bounded length: a sender's absurd one is not shown.
                            let duration = meta.duration;
                            let waveform = waveform_bars(&meta.waveform);
                            let size = c.info.as_ref().and_then(|info| info.size);
                            let summary = attach_media(
                                &mut message,
                                kind,
                                icon,
                                &c.source,
                                c.filename(),
                                c.caption(),
                                size,
                                duration,
                                waveform.as_deref(),
                            );
                            if let Some(file) = &mut message.file {
                                file.audio = Some(std::sync::Arc::new(meta));
                            }
                            summary
                        }
                        MessageType::Location(c) => format_location(c),
                        other => other.body().to_owned(),
                    };
                }
                MsgLikeKind::Redacted => {
                    // A deleted message has no content to doubt.
                    message.shield = None;
                    message.kind = MsgKind::Redacted;
                    message.body = "Message deleted".to_owned();
                    message.deleted = Some(Deletion {
                        reason: deletion_reason(event.redaction_reason()),
                        by: match deleter(event, own) {
                            Deleter::Sender => DeletedBy::Sender,
                            Deleter::Us => DeletedBy::Us,
                            // Named by `receipts::Names`, as readers are.
                            Deleter::Other(user) => DeletedBy::Other {
                                user_id: user.to_string(),
                                name: sanitize_line(user.as_str()),
                            },
                        },
                    });
                }
                MsgLikeKind::UnableToDecrypt(encrypted) => {
                    let why = utd_why(encrypted);
                    message.kind = MsgKind::Undecryptable;
                    message.body = why.text().to_owned();
                    message.utd = Some(why);
                }
                MsgLikeKind::Sticker(sticker) => {
                    let (image, body) = sticker_parts(sticker.content());
                    message.body = body;
                    message.image = Some(image);
                }
                MsgLikeKind::Poll(poll) => {
                    convert_poll(&mut message, poll, event.is_editable(), own, ignored);
                }
                _ => message.body = "[unsupported message]".to_owned(),
            }
        }
        TimelineItemContent::MembershipChange(change) => {
            // Whom it is about, told apart from a namesake: one's own joining or
            // leaving as the sender is; someone else by the name the change gives
            // them (with their ID where a namesake is loaded: see `mark_namesakes`).
            let user = change.user_id();
            let (name, who) = if user == event.sender() {
                (message.sender.clone(), shown.clone())
            } else {
                let name = change
                    .display_name()
                    .map(|name| sanitize_line(&name))
                    .filter(|name| !crate::text::is_blank(name))
                    .unwrap_or_else(|| sanitize_line(user.localpart()));
                (name.clone(), name)
            };
            message.subject = Some((name, user.to_string()));
            let what = match change.change() {
                Some(MembershipChange::Joined | MembershipChange::InvitationAccepted) => {
                    "joined".to_owned()
                }
                Some(MembershipChange::Left) => "left".to_owned(),
                Some(MembershipChange::Invited) => format!("was invited by {shown}"),
                Some(MembershipChange::InvitationRejected) => "declined the invitation".to_owned(),
                Some(MembershipChange::InvitationRevoked) => {
                    "had their invitation withdrawn".to_owned()
                }
                Some(MembershipChange::Kicked) => format!("was removed by {shown}"),
                Some(MembershipChange::Banned | MembershipChange::KickedAndBanned) => {
                    format!("was banned by {shown}")
                }
                Some(MembershipChange::Unbanned) => format!("was unbanned by {shown}"),
                Some(MembershipChange::Knocked) => "asked to join".to_owned(),
                _ => "changed membership".to_owned(),
            };
            system(&mut message, format!("{who} {what}"));
        }
        TimelineItemContent::ProfileChange(_) => {
            message.subject = Some((message.sender.clone(), event.sender().to_string()));
            system(&mut message, format!("{shown} updated their profile"));
        }
        TimelineItemContent::OtherState(state) => {
            let what = state.content().event_type().to_string();
            let what = match what.as_str() {
                "m.room.name" => "the room name".to_owned(),
                "m.room.topic" => "the topic".to_owned(),
                "m.room.avatar" => "the room picture".to_owned(),
                "m.room.encryption" => "encryption (now on)".to_owned(),
                "m.room.create" => {
                    message.subject = Some((message.sender.clone(), event.sender().to_string()));
                    return Some(system_owned(message, format!("{shown} created the room")));
                }
                // Power levels, join rules and the like: noise in a chat view.
                _ => return None,
            };
            message.subject = Some((message.sender.clone(), event.sender().to_string()));
            system(&mut message, format!("{shown} changed {what}"));
        }
        TimelineItemContent::CallInvite | TimelineItemContent::RtcNotification { .. } => {
            message.subject = Some((message.sender.clone(), event.sender().to_string()));
            system(&mut message, format!("{shown} started a call"));
        }
        _ => system(&mut message, "An event this app cannot read".to_owned()),
    }
    // Text from other people goes to the terminal: no control characters.
    message.sender = sanitize_line(&message.sender);
    message.body = sanitize(&message.body);
    if let Some((sender, quoted)) = &mut message.reply_to {
        *sender = sanitize_line(sender);
        *quoted = sanitize_line(quoted);
    }
    for reaction in &mut message.reactions {
        reaction.key = sanitize_line(&reaction.key);
    }
    Some(message)
}

/// Who deleted a deleted event (see [`deleter`]).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Deleter {
    /// Its sender, or someone not known.
    Sender,
    /// We did, someone else's event.
    Us,
    /// Someone else did.
    Other(OwnedUserId),
}

/// Who deleted `event`, which is deleted. While a deletion of ours is on its way, it
/// is ours. Otherwise the server's copy says: a deleted event carries the deletion in
/// `unsigned.redacted_because`, with its sender (the SDK keeps only the reason, see
/// `redaction_reason`). Where it does not say (some servers; a deletion that has just
/// come in, before the event cache's copy), it counts as the sender's.
pub(super) fn deleter(event: &EventTimelineItem, own: &UserId) -> Deleter {
    #[derive(serde::Deserialize)]
    struct Unsigned {
        redacted_because: Option<Because>,
    }
    #[derive(serde::Deserialize)]
    struct Because {
        sender: Option<OwnedUserId>,
    }
    if event.redaction_send_state().is_some() {
        return if event.sender() == own {
            Deleter::Sender
        } else {
            Deleter::Us
        };
    }
    let said = event
        .original_json()
        .and_then(|raw| raw.get_field::<Unsigned>("unsigned").ok().flatten())
        .and_then(|unsigned| unsigned.redacted_because?.sender);
    match said {
        Some(user) if user == event.sender() => Deleter::Sender,
        Some(user) if user == own => Deleter::Us,
        Some(user) => Deleter::Other(user),
        None => Deleter::Sender,
    }
}

/// The reason given for a deletion, made safe for one line: no control characters, no
/// direction overrides, one space where there were several, at most
/// [`Deletion::REASON_MAX`] columns. `None` for one that shows nothing.
fn deletion_reason(reason: Option<&str>) -> Option<String> {
    let line = sanitize_line(reason?);
    let line = line
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if crate::text::is_blank(&line) {
        return None;
    }
    Some(crate::text::truncate(&line, Deletion::REASON_MAX))
}

/// What the SDK advises to warn about, in its lenient mode: people who have never been
/// verified are not marked (most contacts never are), only what contradicts an
/// expectation. "Authenticity not guaranteed" covers harmless history restored from
/// the backup as well as a device that could not be tied to its owner; the SDK does
/// not tell them apart, so it is shown, as a quiet remark rather than a warning.
fn shield(event: &EventTimelineItem) -> Option<Shield> {
    use matrix_sdk_ui::timeline::TimelineEventShieldState as State;
    use matrix_sdk_ui::timeline::TimelineEventShieldStateCode as Code;
    let (State::Red { code } | State::Grey { code }) = event.get_shield(false) else {
        return None;
    };
    match code {
        Code::UnknownDevice | Code::UnsignedDevice => Some(Shield::UnverifiedDevice),
        Code::VerificationViolation => Some(Shield::IdentityChanged),
        Code::MismatchedSender => Some(Shield::SenderMismatch),
        Code::SentInClear => Some(Shield::NotEncrypted),
        Code::AuthenticityNotGuaranteed => Some(Shield::Unconfirmed),
        // Never produced in lenient mode.
        Code::UnverifiedIdentity => None,
    }
}

fn media_source_key(source: &MediaSource) -> String {
    match source {
        MediaSource::Plain(uri) => uri.to_string(),
        MediaSource::Encrypted(file) => file.url.to_string(),
    }
}

fn parse_dimensions(width: Option<UInt>, height: Option<UInt>) -> Option<(u32, u32)> {
    let width = u32::try_from(u64::from(width?)).ok()?;
    let height = u32::try_from(u64::from(height?)).ok()?;
    (width > 0 && height > 0).then_some((width, height))
}

fn format_duration(duration: std::time::Duration) -> String {
    crate::audio::length(duration)
}

/// The line of bars under a voice message; `None` without a waveform.
fn waveform_bars(waveform: &[u16]) -> Option<String> {
    (!waveform.is_empty()).then(|| crate::audio::bars(waveform))
}

/// What a voice or audio message says about its sound, bounded: a believable length,
/// a short waveform, and its type (only ever used to pick a file ending).
fn audio_meta(content: &AudioMessageEventContent) -> AudioMeta {
    let stated = content
        .info
        .as_ref()
        .and_then(|info| info.duration)
        .or_else(|| content.audio.as_ref().map(|audio| audio.duration));
    // As many as the event holds, which is bounded by the size of an event.
    let amplitudes: Vec<u64> = content
        .audio
        .as_ref()
        .map(|audio| {
            audio
                .waveform
                .iter()
                .map(|sample| u64::from(sample.get()))
                .collect()
        })
        .unwrap_or_default();
    AudioMeta {
        duration: stated.and_then(crate::audio::bounded_duration),
        waveform: crate::audio::bounded_waveform(&amplitudes),
        mime: content
            .info
            .as_ref()
            .and_then(|info| info.mimetype.as_deref())
            .map(|mime| sanitize_line(mime).chars().take(100).collect()),
    }
}

pub(super) fn location_label(c: &LocationMessageEventContent) -> String {
    let uri = sanitize_line(c.geo_uri());
    let desc = c
        .location
        .as_ref()
        .and_then(|loc| loc.description.as_deref())
        .map(sanitize_line)
        .filter(|d| !d.trim().is_empty())
        .or_else(|| {
            let plain = sanitize_line(c.plain_text_representation());
            (!plain.trim().is_empty() && plain != uri).then_some(plain)
        });
    if let Some(desc) = desc {
        return desc;
    }
    if crate::rich::safe_geo_uri(&uri) {
        return uri;
    }
    "Location".to_owned()
}

fn format_location(c: &LocationMessageEventContent) -> String {
    let uri = sanitize_line(c.geo_uri());
    let safe_uri = crate::rich::safe_geo_uri(&uri);
    let desc = c
        .location
        .as_ref()
        .and_then(|loc| loc.description.as_deref())
        .map(sanitize_line)
        .filter(|d| !d.trim().is_empty())
        .or_else(|| {
            let plain = sanitize_line(c.plain_text_representation());
            (!plain.trim().is_empty() && plain != uri).then_some(plain)
        });
    match (desc, safe_uri) {
        (Some(desc), true) if desc.contains(&uri) => format!("📍 {desc}"),
        (Some(desc), true) => format!("📍 {desc} · {uri}"),
        (Some(desc), false) => format!("📍 {desc}"),
        (None, true) => format!("📍 {uri}"),
        (None, false) => "📍 Location".to_owned(),
    }
}

/// Marks a message as carrying a file or playable media and returns the summary text.
#[allow(clippy::too_many_arguments)]
fn attach_media(
    message: &mut Message,
    kind: MediaKind,
    icon: &str,
    source: &MediaSource,
    filename: &str,
    caption: Option<&str>,
    size: Option<UInt>,
    duration: Option<std::time::Duration>,
    waveform: Option<&str>,
) -> String {
    let key = media_source_key(source);
    let name = sanitize_line(filename);
    let name = if name.trim().is_empty() {
        match kind {
            MediaKind::Voice => "voice message",
            MediaKind::Audio => "audio",
            MediaKind::Video => "video",
            MediaKind::File | MediaKind::Picture => "attachment",
        }
        .to_owned()
    } else {
        name
    };
    let size = size.map(u64::from);
    let mut text = format!("{icon} {name}");
    if let Some(duration) = duration {
        text.push_str(" · ");
        text.push_str(&format_duration(duration));
    }
    if let Some(size) = size {
        text.push_str(" · ");
        text.push_str(&crate::images::human_size(size));
    }
    if let Some(bars) = waveform {
        text.push('\n');
        text.push_str(bars);
    }
    if let Some(caption) = caption.map(sanitize).filter(|c| !c.trim().is_empty()) {
        text.push('\n');
        text.push_str(&caption);
    }
    message.file = Some(Attachment {
        key: sanitize_line(&key),
        name,
        size,
        kind,
        source: Box::new(source.clone()),
        audio: None,
    });
    text
}

/// What the UI needs to show a picture. Everything in it comes from the sender.
fn image_info(
    source: MediaSource,
    filename: &str,
    caption: Option<&str>,
    info: Option<&RumaImageInfo>,
) -> ImageInfo {
    let key = media_source_key(&source);
    let name = sanitize_line(filename);
    let dimensions = info.and_then(|info| parse_dimensions(info.width, info.height));
    let thumbnail = info.and_then(|info| {
        let size = info
            .thumbnail_info
            .as_ref()
            .and_then(|thumbnail| thumbnail.size)
            .map(u64::from);
        Some((info.thumbnail_source.clone()?, size))
    });
    ImageInfo {
        key: sanitize_line(&key),
        source: ImageSource::Matrix(Box::new(MediaRef {
            full: source,
            thumbnail,
        })),
        name: if name.trim().is_empty() {
            "image".to_owned()
        } else {
            name
        },
        caption: caption
            .map(sanitize)
            .filter(|caption| !caption.trim().is_empty()),
        dimensions,
        size: info.and_then(|info| info.size).map(u64::from),
        sticker: false,
    }
}

/// A sticker's picture, marked as one, and the line that stands for it where pictures
/// are not drawn. Its name is the event's `body`, made safe like a file's name.
fn sticker_parts(
    content: &matrix_sdk::ruma::events::sticker::StickerEventContent,
) -> (ImageInfo, String) {
    let mut image = image_info(
        MediaSource::from(content.source.clone()),
        &content.body,
        None,
        Some(&content.info),
    );
    image.sticker = true;
    let body = format!("[sticker] {}", image.name);
    (image, body)
}

/// The text shown instead of a picture where pictures are not drawn.
fn image_fallback(image: &ImageInfo) -> String {
    match &image.caption {
        Some(caption) => format!("🖼  {}\n{caption}", image.name),
        None => format!("🖼  {}", image.name),
    }
}

fn system(message: &mut Message, text: String) {
    message.kind = MsgKind::System;
    message.body = text;
}

fn system_owned(mut message: Message, text: String) -> Message {
    system(&mut message, text);
    message
}

fn convert_poll(
    message: &mut Message,
    poll: &matrix_sdk_ui::timeline::PollState,
    editable: bool,
    own: &UserId,
    ignored: &HashSet<OwnedUserId>,
) {
    let results = poll.results();
    let disclosed = matches!(
        results.kind,
        matrix_sdk::ruma::events::poll::start::PollKind::Disclosed
    );
    let answers: Vec<(String, String)> = results
        .answers
        .into_iter()
        .map(|answer| (answer.id, answer.text))
        .collect();
    convert_poll_parts(
        message,
        &results.question,
        disclosed,
        results.max_selections,
        answers,
        &results.votes,
        results.end_time.is_some(),
        results.has_been_edited,
        editable,
        own,
        ignored,
    );
}

#[allow(clippy::too_many_arguments)]
fn convert_poll_parts(
    message: &mut Message,
    raw_question: &str,
    disclosed: bool,
    max_selections: u64,
    raw_answers: Vec<(String, String)>,
    votes_by_answer: &std::collections::HashMap<String, Vec<OwnedUserId>>,
    ended: bool,
    has_been_edited: bool,
    editable: bool,
    own: &UserId,
    ignored: &HashSet<OwnedUserId>,
) {
    // A poll is somebody else's event: more answers than a poll may have are dropped,
    // as are answers whose id is no id, and long texts are cut.
    const TEXT_CHARS: usize = 600;
    const ID_BYTES: usize = 255;
    let cut = |text: &str| -> String {
        sanitize_line(text.trim())
            .chars()
            .take(TEXT_CHARS)
            .collect()
    };
    let question = cut(raw_question);
    let mut voters: HashSet<&UserId> = HashSet::new();
    let answers: Vec<crate::model::PollAnswer> = raw_answers
        .into_iter()
        .filter(|(id, _)| !id.is_empty() && id.len() <= ID_BYTES)
        .take(crate::modal::MAX_POLL_ANSWERS)
        .map(|(id, text)| {
            let mut votes = 0u32;
            let mut own_voted = false;
            if let Some(list) = votes_by_answer.get(&id) {
                for uid in list {
                    if ignored.contains(uid) {
                        continue;
                    }
                    votes = votes.saturating_add(1);
                    if uid == own {
                        own_voted = true;
                    }
                    voters.insert(uid);
                }
            }
            crate::model::PollAnswer {
                id,
                text: cut(&text),
                votes,
                own: own_voted,
            }
        })
        .collect();

    // Never include vote counts or voter totals in the plain-text fallback body so
    // undisclosed polls cannot leak results through search, quotes or plain rendering.
    let options: Vec<&str> = answers
        .iter()
        .map(|a| a.text.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    message.body = if options.is_empty() {
        format!("📊 {question}")
    } else {
        format!("📊 {question} ({})", options.join(" / "))
    };
    message.edited |= has_been_edited;
    message.poll = Some(crate::model::PollInfo {
        question,
        disclosed,
        max_selections,
        answers,
        total_voters: voters.len() as u32,
        ended,
        editable: editable && !ended,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn every_cause_the_sdk_knows_gets_its_own_words() {
        #[allow(deprecated)]
        let megolm = |cause| EncryptedMessage::MegolmV1AesSha2 {
            sender_key: None,
            device_id: None,
            session_id: "session".to_owned(),
            cause,
        };
        let causes = [
            (UtdCause::Unknown, UtdWhy::NoKey),
            (UtdCause::SentBeforeWeJoined, UtdWhy::SentBeforeJoin),
            (
                UtdCause::VerificationViolation,
                UtdWhy::SenderIdentityChanged,
            ),
            (UtdCause::UnsignedDevice, UtdWhy::SenderDeviceUnsigned),
            (UtdCause::UnknownDevice, UtdWhy::SenderDeviceUnknown),
            (
                UtdCause::HistoricalMessageAndBackupIsDisabled,
                UtdWhy::HistoryNoBackup,
            ),
            (
                UtdCause::HistoricalMessageAndDeviceIsUnverified,
                UtdWhy::HistoryNeedsVerification,
            ),
            (
                UtdCause::WithheldForUnverifiedOrInsecureDevice,
                UtdWhy::WithheldUnverified,
            ),
            (UtdCause::WithheldBySender, UtdWhy::Withheld),
            (UtdCause::Malformed, UtdWhy::Malformed),
        ];
        let mut texts = HashSet::new();
        for (cause, why) in causes {
            assert_eq!(utd_why(&megolm(cause)), why);
            // One line each, different from all others, and none names an id.
            let text = why.text();
            assert!(texts.insert(text), "{text}");
            assert!(!text.contains('\n') && !text.contains("session"), "{text}");
            assert!(!why.advice().is_empty());
        }
        assert_eq!(utd_why(&EncryptedMessage::Unknown), UtdWhy::Unsupported);
        assert_eq!(
            utd_why(&EncryptedMessage::OlmV1Curve25519AesSha2 {
                sender_key: "key".to_owned()
            }),
            UtdWhy::Unsupported
        );
        assert!(texts.insert(UtdWhy::Unsupported.text()));
    }

    #[test]
    fn image_info_is_taken_from_the_event_and_made_safe() {
        use matrix_sdk::ruma::events::room::ThumbnailInfo;
        use matrix_sdk::ruma::{UInt, mxc_uri};

        let mut info = RumaImageInfo::new();
        info.width = UInt::new(640);
        info.height = UInt::new(480);
        info.size = UInt::new(12_345);
        let mut thumbnail = ThumbnailInfo::new();
        thumbnail.size = UInt::new(900);
        info.thumbnail_info = Some(Box::new(thumbnail));
        info.thumbnail_source = Some(MediaSource::Plain(
            mxc_uri!("mxc://example.org/thumb").to_owned(),
        ));
        let source = MediaSource::Plain(mxc_uri!("mxc://example.org/abc").to_owned());
        let image = image_info(
            source.clone(),
            "cat\u{1b}[31m.png",
            Some("look\u{7} at it"),
            Some(&info),
        );
        assert_eq!(image.key, "mxc://example.org/abc");
        assert_eq!(image.name, "cat[31m.png");
        assert_eq!(image.caption.as_deref(), Some("look at it"));
        assert_eq!(image.dimensions, Some((640, 480)));
        assert_eq!(image.size, Some(12_345));
        let ImageSource::Matrix(media) = &image.source else {
            panic!("not a Matrix source");
        };
        assert!(matches!(
            media.thumbnail,
            Some((MediaSource::Plain(_), Some(900)))
        ));
        assert_eq!(image_fallback(&image), "🖼  cat[31m.png\nlook at it");
        // The key of an encrypted file must not show up in debug output.
        assert_eq!(format!("{:?}", image.source), "Matrix");

        // No info at all: no dimensions, and an empty name gets a stand-in.
        let bare = image_info(source, " ", None, None);
        assert_eq!(bare.name, "image");
        assert_eq!(bare.dimensions, None);
        assert!(bare.caption.is_none());
        assert_eq!(image_fallback(&bare), "🖼  image");

        // Nonsense dimensions are ignored instead of trusted.
        let mut zero = RumaImageInfo::new();
        zero.width = UInt::new(0);
        zero.height = UInt::new(10);
        let source = MediaSource::Plain(mxc_uri!("mxc://example.org/abc").to_owned());
        assert_eq!(image_info(source, "a", None, Some(&zero)).dimensions, None);
    }

    #[test]
    fn a_sticker_is_a_small_picture_named_by_its_body_made_safe() {
        use matrix_sdk::ruma::events::sticker::StickerEventContent;
        use matrix_sdk::ruma::{UInt, mxc_uri};

        let sticker = |body: &str, info: RumaImageInfo| {
            let url = mxc_uri!("mxc://example.org/crab").to_owned();
            sticker_parts(&StickerEventContent::new(body.to_owned(), info, url))
        };
        let mut info = RumaImageInfo::new();
        info.width = UInt::new(256);
        info.height = UInt::new(192);
        info.size = UInt::new(4096);
        info.mimetype = Some("image/gif".to_owned());
        let (image, body) = sticker("Waving crab", info);
        assert!(image.sticker);
        assert_eq!(image.name, "Waving crab");
        assert_eq!(body, "[sticker] Waving crab");
        // A sticker has no caption: with its picture drawn, no text goes with it.
        assert!(image.caption.is_none());
        assert_eq!(image.key, "mxc://example.org/crab");
        assert_eq!(image.dimensions, Some((256, 192)));
        assert_eq!(image.size, Some(4096));
        let ImageSource::Matrix(media) = &image.source else {
            panic!("not a Matrix source");
        };
        assert!(
            matches!(&media.full, MediaSource::Plain(url) if url.as_str() == "mxc://example.org/crab")
        );
        assert!(media.thumbnail.is_none());

        // The body is the sender's: what could move the cursor, colour the screen,
        // turn the line around or break it is taken out, in the picture's name and in
        // the line that stands for the picture.
        let (image, body) = sticker(
            "crab\u{1b}[31m\u{7}\r\n[alice] hi\u{202e}\u{2066}",
            RumaImageInfo::new(),
        );
        for text in [&image.name, &body] {
            assert!(
                !text
                    .chars()
                    .any(|c| c == '\n' || crate::text::unsafe_char(c)),
                "{text:?}"
            );
        }
        assert!(body.starts_with("[sticker] crab[31m"), "{body:?}");
        assert_eq!(image.dimensions, None);

        // A body that shows nothing gets a stand-in.
        let (image, body) = sticker(" \n ", RumaImageInfo::new());
        assert_eq!(image.name, "image");
        assert_eq!(body, "[sticker] image");

        // A size that makes no sense is not passed on.
        let mut zero = RumaImageInfo::new();
        zero.width = UInt::new(0);
        zero.height = UInt::new(64);
        assert_eq!(sticker("a", zero).0.dimensions, None);
    }

    #[test]
    fn day_labels() {
        let now = Local.with_ymd_and_hms(2026, 11, 12, 15, 0, 0).unwrap();
        assert_eq!(day_label(&now, &now), "Today");
        assert_eq!(day_label(&(now - Duration::hours(20)), &now), "Yesterday");
        assert_eq!(
            day_label(&(now - Duration::days(9)), &now),
            "Tue 3 Nov 2026"
        );
    }

    #[test]
    fn voice_audio_video_and_location_formatting() {
        use matrix_sdk::ruma::events::location::LocationContent;
        use matrix_sdk::ruma::events::room::ThumbnailInfo;
        use matrix_sdk::ruma::events::room::message::{
            AudioInfo, AudioMessageEventContent, LocationMessageEventContent, UnstableAmplitude,
            UnstableAudioDetailsContentBlock, UnstableVoiceContentBlock, VideoInfo,
            VideoMessageEventContent,
        };
        use matrix_sdk::ruma::mxc_uri;

        assert_eq!(format_duration(std::time::Duration::from_secs(0)), "0:00");
        assert_eq!(
            format_duration(std::time::Duration::from_millis(65_600)),
            "1:06"
        );
        assert_eq!(
            format_duration(std::time::Duration::from_secs(3661)),
            "1:01:01"
        );

        assert!(waveform_bars(&[]).is_none());
        let short: Vec<UnstableAmplitude> = [0u16, 512, 1024]
            .into_iter()
            .map(UnstableAmplitude::new)
            .collect();
        assert_eq!(waveform_bars(&[0, 512, 1024]).as_deref(), Some("▁▄█"));
        let long: Vec<u16> = (0u16..48).map(|n| (n * 21) % 1024).collect();
        assert_eq!(waveform_bars(&long).unwrap().chars().count(), 24);

        let mut voice_msg = Message::default();
        let mut voice_info = AudioInfo::new();
        voice_info.size = UInt::new(2_000);
        voice_info.duration = Some(std::time::Duration::from_secs(12));
        let mut voice_content = AudioMessageEventContent::plain(
            "voice.ogg".to_owned(),
            mxc_uri!("mxc://example.org/v").to_owned(),
        );
        voice_content.info = Some(Box::new(voice_info));
        voice_content.audio = Some(UnstableAudioDetailsContentBlock::new(
            std::time::Duration::from_secs(12),
            short.clone(),
        ));
        voice_content.voice = Some(UnstableVoiceContentBlock::new());
        let body = attach_media(
            &mut voice_msg,
            MediaKind::Voice,
            "🎤",
            &voice_content.source,
            voice_content.filename(),
            voice_content.caption(),
            voice_content.info.as_ref().and_then(|i| i.size),
            voice_content
                .info
                .as_ref()
                .and_then(|i| i.duration)
                .or_else(|| voice_content.audio.as_ref().map(|a| a.duration)),
            waveform_bars(&audio_meta(&voice_content).waveform).as_deref(),
        );
        assert_eq!(body, "🎤 voice.ogg · 0:12 · 2.0 kB\n▁▄█");
        // What is kept for drawing how far it has played.
        let mut info = AudioInfo::new();
        info.mimetype = Some("audio/ogg\u{1b}[31m".to_owned());
        voice_content.info = Some(Box::new(info));
        assert_eq!(
            audio_meta(&voice_content),
            AudioMeta {
                duration: Some(std::time::Duration::from_secs(12)),
                waveform: vec![0, 512, 1024],
                mime: Some("audio/ogg[31m".to_owned()),
            }
        );
        // A sender's word is bounded: no endless length, no endless waveform.
        voice_content.audio = Some(UnstableAudioDetailsContentBlock::new(
            std::time::Duration::from_secs(10_000_000),
            (0..5000u16)
                .map(|n| UnstableAmplitude::new(n % 2000))
                .collect(),
        ));
        let bounded = audio_meta(&voice_content);
        assert_eq!(bounded.duration, None);
        assert_eq!(bounded.waveform.len(), crate::audio::MAX_WAVEFORM);
        assert!(bounded.waveform.iter().all(|value| *value <= 1024));
        assert!(voice_msg.is_playable());
        assert_eq!(voice_msg.file.as_ref().unwrap().kind, MediaKind::Voice);

        let mut audio_msg = Message::default();
        let audio_content = AudioMessageEventContent::plain(
            "song.flac".to_owned(),
            mxc_uri!("mxc://example.org/a").to_owned(),
        );
        let audio_body = attach_media(
            &mut audio_msg,
            MediaKind::Audio,
            "🔊",
            &audio_content.source,
            audio_content.filename(),
            audio_content.caption(),
            None,
            None,
            None,
        );
        assert_eq!(audio_body, "🔊 song.flac");
        assert_eq!(audio_msg.file.as_ref().unwrap().kind, MediaKind::Audio);

        let mut video_info = VideoInfo::new();
        video_info.duration = Some(std::time::Duration::from_secs(90));
        video_info.size = UInt::new(4_000_000);
        let mut tinfo = ThumbnailInfo::new();
        tinfo.width = UInt::new(320);
        tinfo.height = UInt::new(180);
        video_info.thumbnail_info = Some(Box::new(tinfo));
        video_info.thumbnail_source = Some(MediaSource::Plain(
            mxc_uri!("mxc://example.org/vthumb").to_owned(),
        ));
        let mut video = VideoMessageEventContent::plain(
            "clip.mp4".to_owned(),
            mxc_uri!("mxc://example.org/vid").to_owned(),
        );
        video.info = Some(Box::new(video_info));
        assert_eq!(
            format_duration(video.info.as_ref().unwrap().duration.unwrap()),
            "1:30"
        );
        // Location formatting respects description + validates geo URIs.
        let mut loc =
            LocationMessageEventContent::new("London".to_owned(), "geo:51.5074,-0.1278".to_owned());
        loc.location = Some(LocationContent::new("geo:51.5074,-0.1278".to_owned()));
        if let Some(ref mut inner) = loc.location {
            inner.description = Some("Big\u{1b}[31m Ben".to_owned());
        }
        assert_eq!(
            format_location(&loc),
            "📍 Big[31m Ben · geo:51.5074,-0.1278"
        );

        let bad_loc =
            LocationMessageEventContent::new("Trap".to_owned(), "javascript:alert(1)".to_owned());
        assert_eq!(format_location(&bad_loc), "📍 Trap");
    }

    #[test]
    fn a_poll_made_to_be_huge_is_cut_down() {
        let own = matrix_sdk::ruma::user_id!("@me:example.org").to_owned();
        let mut answers: Vec<(String, String)> = (0..500)
            .map(|n| (format!("a{n}"), format!("Answer {n}")))
            .collect();
        answers.insert(0, (String::new(), "no id".into()));
        answers.insert(1, ("x".repeat(300), "a huge id".into()));
        answers.insert(2, ("long".into(), "y".repeat(5000)));
        let mut message = Message::default();
        convert_poll_parts(
            &mut message,
            &"q".repeat(5000),
            true,
            1,
            answers,
            &std::collections::HashMap::new(),
            false,
            false,
            false,
            &own,
            &HashSet::new(),
        );
        let poll = message.poll.as_ref().unwrap();
        assert_eq!(poll.answers.len(), crate::modal::MAX_POLL_ANSWERS);
        assert_eq!(poll.answers[0].id, "long");
        assert_eq!(poll.answers[0].text.chars().count(), 600);
        assert_eq!(poll.question.chars().count(), 600);
    }

    #[test]
    fn poll_conversion_respects_disclosed_ignored_voters_and_editions() {
        use matrix_sdk::ruma::owned_user_id;
        use std::collections::HashMap;

        let own = owned_user_id!("@alice:example.org");
        let bob = owned_user_id!("@bob:example.org");
        let eve = owned_user_id!("@eve:example.org");
        let ignored = HashSet::from([eve.clone()]);

        let mut votes = HashMap::new();
        votes.insert("a1".to_owned(), vec![own.clone(), eve.clone()]);
        votes.insert("a2".to_owned(), vec![bob.clone(), eve.clone()]);

        let mut msg = Message {
            own: true,
            ..Message::default()
        };
        convert_poll_parts(
            &mut msg,
            "Best\u{1b}[31m snack?\nPick one",
            false,
            2,
            vec![
                ("a1".into(), "Chips\x07".into()),
                ("a2".into(), "Fruit".into()),
            ],
            &votes,
            false,
            true,
            false,
            &own,
            &ignored,
        );
        let p = msg.poll.as_ref().expect("poll populated");
        assert!(msg.edited);
        assert_eq!(p.question, "Best[31m snack? Pick one");
        assert!(!p.disclosed);
        assert!(!p.shows_counts());
        assert!(p.is_multi());
        assert!(!msg.editable());
        assert_eq!(p.answers[0].text, "Chips");
        assert_eq!(p.answers[0].votes, 1);
        assert!(p.answers[0].own);
        assert_eq!(p.answers[1].votes, 1);
        assert!(!p.answers[1].own);
        assert_eq!(p.total_voters, 2);
        // Plain fallback must never reveal vote counts for an open undisclosed poll.
        assert!(!msg.body.contains("1 vote"));
        assert!(msg.body.contains("Best[31m snack? Pick one"));

        let mut ended_msg = Message {
            own: true,
            ..Message::default()
        };
        convert_poll_parts(
            &mut ended_msg,
            "Winner?",
            false,
            1,
            vec![("a1".into(), "One".into()), ("a2".into(), "Two".into())],
            &votes,
            true,
            false,
            true,
            &own,
            &ignored,
        );
        let ended_p = ended_msg.poll.as_ref().unwrap();
        assert!(ended_p.ended);
        assert!(ended_p.shows_counts());
        assert!(!ended_msg.editable());
        assert_eq!(ended_p.winner_max(), 1);
    }

    fn test_event(uid: &str, event: &str, body: &str) -> Message {
        Message {
            id: uid.to_owned(),
            item: Some(ItemRef::Event(event.to_owned())),
            body: body.to_owned(),
            day: "Today".to_owned(),
            ..Message::default()
        }
    }

    fn test_marker(uid: &str) -> Message {
        Message {
            id: uid.to_owned(),
            kind: MsgKind::ReadMarker,
            ..Message::default()
        }
    }

    fn summary(messages: &[Message]) -> Vec<(&str, Option<&str>, &str)> {
        messages
            .iter()
            .map(|m| {
                let key = match m.item.as_ref() {
                    Some(ItemRef::Event(id)) => Some(id.as_str()),
                    Some(ItemRef::Transaction(id)) => Some(id.as_str()),
                    None => None,
                };
                (m.id.as_str(), key, m.body.as_str())
            })
            .collect()
    }

    #[test]
    fn dedup_collapses_repeated_block_with_latest_content() {
        use crate::model::{ReadBy, Reader};

        let stale_reader = ReadBy {
            readers: vec![Reader {
                name: "Bob".to_owned(),
                time: "12:00".to_owned(),
                ..Default::default()
            }],
            total: 1,
        };
        let fresh_reader = ReadBy {
            readers: vec![Reader {
                name: "Carol".to_owned(),
                time: "12:05".to_owned(),
                ..Default::default()
            }],
            total: 1,
        };

        let mut e471_first = test_event("u1", "$471", "original 471");
        e471_first.read_by = stale_reader;
        let mut e471_later = test_event("u6", "$471", "edited 471");
        e471_later.edited = true;
        e471_later.reactions = vec![Reaction {
            key: "👍".to_owned(),
            count: 1,
            own: true,
            failed: false,
        }];
        // SDK moved the receipt off the trailing copy; copy 1 held a stale receipt.
        e471_later.read_by = ReadBy::default();

        let e472_first = test_event("u2", "$472", "v1 472");
        let e472_middle = test_event("u7", "$472", "v2 472");
        let mut e472_last = test_event("u11", "$472", "v3 472");
        e472_last.read_by = fresh_reader.clone();

        let e473_first = test_event("u3", "$473", "473");
        let mut e473_later = test_event("u8", "$473", "Message deleted");
        e473_later.kind = MsgKind::Redacted;

        let mut messages = vec![
            test_event("u0", "$470", "470"),
            e471_first,
            e472_first,
            e473_first,
            test_event("u4", "$474", "474 v1"),
            e471_later,
            e472_middle,
            e473_later,
            test_event("u9", "$474", "474 v2"),
            test_event("u10", "$475", "475"),
            e472_last,
        ];

        let mut dedup = EventDedup::default();
        dedup.dedup(&mut messages);

        assert_eq!(
            summary(&messages),
            vec![
                ("u0", Some("$470"), "470"),
                ("u1", Some("$471"), "edited 471"),
                ("u2", Some("$472"), "v3 472"),
                ("u3", Some("$473"), "Message deleted"),
                ("u4", Some("$474"), "474 v2"),
                ("u10", Some("$475"), "475"),
            ]
        );
        assert!(messages[1].edited);
        assert_eq!(messages[1].reactions.len(), 1);
        assert!(messages[1].read_by.is_empty());
        assert_eq!(messages[2].read_by, fresh_reader);
        assert_eq!(messages[3].kind, MsgKind::Redacted);
    }

    /// Of an event held twice, the SDK decrypts the copy it finds by position when keys
    /// come in from a key file, which can be the first: the second, still locked, does
    /// not hide it. Decrypted last, the last copy wins as before.
    #[test]
    fn dedup_keeps_the_decrypted_copy_of_an_event() {
        let locked = |id: &str, event: &str| {
            let mut message = test_event(id, event, "Unable to decrypt");
            message.kind = MsgKind::Undecryptable;
            message
        };
        let mut messages = vec![
            test_event("u1", "$1", "readable now"),
            locked("u2", "$2"),
            locked("u3", "$1"),
            test_event("u4", "$2", "readable too"),
        ];
        EventDedup::default().dedup(&mut messages);
        assert_eq!(
            summary(&messages),
            vec![
                ("u1", Some("$1"), "readable now"),
                ("u2", Some("$2"), "readable too"),
            ]
        );
        assert!(
            messages
                .iter()
                .all(|message| message.kind != MsgKind::Undecryptable)
        );
    }

    #[test]
    fn dedup_keeps_non_event_items_and_clean_list_untouched() {
        let local = |uid: &str, txn: &str, body: &str| Message {
            id: uid.to_owned(),
            item: Some(ItemRef::Transaction(txn.to_owned())),
            body: body.to_owned(),
            ..Message::default()
        };
        let virtual_msg = |uid: &str, body: &str| Message {
            id: uid.to_owned(),
            item: None,
            body: body.to_owned(),
            ..Message::default()
        };

        let original = vec![
            test_event("u0", "$1", "one"),
            test_marker("rm"),
            test_event("u1", "$2", "two"),
            virtual_msg("v0", "virtual"),
            local("l0", "txn-a", "sending a"),
            local("l1", "txn-b", "sending b"),
        ];
        let mut messages = original.clone();
        let mut dedup = EventDedup::default();
        dedup.dedup(&mut messages);
        assert_eq!(summary(&messages), summary(&original));

        // Reusing the scratch buffer on another repeat-free update does not reallocate it.
        let cap_before = dedup.seen.capacity();
        assert!(cap_before >= original.len());
        dedup.dedup(&mut messages);
        assert_eq!(dedup.seen.capacity(), cap_before);
        assert_eq!(summary(&messages), summary(&original));

        // Even if the slow path runs (as after a hash collision), a unique list stays intact.
        dedup_with_latest(&mut messages);
        assert_eq!(summary(&messages), summary(&original));

        // Non-event items stay untouched when another event in the list repeats.
        let mut mixed = vec![
            test_event("u0", "$1", "one old"),
            virtual_msg("v0", "virtual 1"),
            test_event("u1", "$1", "one new"),
            virtual_msg("v1", "virtual 2"),
            local("l0", "txn-a", "echo 1"),
            local("l1", "txn-a", "echo 2"),
        ];
        dedup.dedup(&mut mixed);
        assert_eq!(
            summary(&mixed),
            vec![
                ("u0", Some("$1"), "one new"),
                ("v0", None, "virtual 1"),
                ("v1", None, "virtual 2"),
                ("l0", Some("txn-a"), "echo 1"),
                ("l1", Some("txn-a"), "echo 2"),
            ]
        );
    }

    #[test]
    fn dedup_keeps_read_marker_after_its_anchor_event() {
        let mut dedup = EventDedup::default();
        let run = |dedup: &mut EventDedup, mut list: Vec<Message>| -> Vec<String> {
            dedup.dedup(&mut list);
            list.into_iter()
                .map(|m| match m.kind {
                    MsgKind::ReadMarker => "RM".to_owned(),
                    _ => m.body,
                })
                .collect()
        };

        // ReadMarker before the repeated block stays before it.
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_event("u0", "$470", "470"),
                    test_marker("rm"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$472", "472"),
                    test_event("u3", "$471", "471'"),
                    test_event("u4", "$472", "472'"),
                    test_event("u5", "$473", "473"),
                ],
            ),
            ["470", "RM", "471'", "472'", "473"]
        );

        // ReadMarker placed by the SDK between copy 1 and copy 2 (fully_read = $472 before
        // replay): stays right after $472', before $473 (or trailing when nothing follows).
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_event("u0", "$470", "470"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$472", "472"),
                    test_marker("rm"),
                    test_event("u3", "$471", "471'"),
                    test_event("u4", "$472", "472'"),
                    test_event("u5", "$473", "473"),
                ],
            ),
            ["470", "471'", "472'", "RM", "473"]
        );
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_event("u0", "$470", "470"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$472", "472"),
                    test_marker("rm"),
                    test_event("u3", "$471", "471'"),
                    test_event("u4", "$472", "472'"),
                ],
            ),
            ["470", "471'", "472'", "RM"]
        );

        // ReadMarker placed inside copy 2 (set_fully_read_event($472) after replay):
        // relocates right after surviving $472', whether or not $475 follows.
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_event("u0", "$470", "470"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$472", "472"),
                    test_event("u3", "$473", "473"),
                    test_event("u4", "$474", "474"),
                    test_event("u5", "$471", "471'"),
                    test_event("u6", "$472", "472'"),
                    test_marker("rm"),
                    test_event("u7", "$473", "473'"),
                    test_event("u8", "$474", "474'"),
                    test_event("u9", "$475", "475"),
                ],
            ),
            ["470", "471'", "472'", "RM", "473'", "474'", "475"]
        );
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_event("u0", "$470", "470"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$472", "472"),
                    test_event("u3", "$473", "473"),
                    test_event("u4", "$474", "474"),
                    test_event("u5", "$471", "471'"),
                    test_event("u6", "$472", "472'"),
                    test_marker("rm"),
                    test_event("u7", "$473", "473'"),
                    test_event("u8", "$474", "474'"),
                ],
            ),
            ["470", "471'", "472'", "RM", "473'", "474'"]
        );

        // ReadMarker immediately after copy 2, after the block, or at index 0.
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_event("u0", "$470", "470"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$472", "472"),
                    test_event("u3", "$471", "471'"),
                    test_event("u4", "$472", "472'"),
                    test_marker("rm"),
                    test_event("u5", "$473", "473"),
                ],
            ),
            ["470", "471'", "472'", "RM", "473"]
        );
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_event("u0", "$470", "470"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$471", "471'"),
                    test_event("u3", "$472", "472"),
                    test_marker("rm"),
                    test_event("u4", "$473", "473"),
                ],
            ),
            ["470", "471'", "472", "RM", "473"]
        );
        assert_eq!(
            run(
                &mut dedup,
                vec![
                    test_marker("rm"),
                    test_event("u1", "$471", "471"),
                    test_event("u2", "$472", "472"),
                    test_event("u3", "$471", "471'"),
                    test_event("u4", "$472", "472'"),
                ],
            ),
            ["RM", "471'", "472'"]
        );
    }
}
