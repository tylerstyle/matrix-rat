//! Turning SDK timeline items into what the UI shows.

use std::collections::HashSet;
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
    Attachment, AudioMeta, ImageInfo, ImageSource, ItemRef, MediaKind, MediaRef, Message, MsgKind,
    Reaction, SendState, Shield, UtdWhy,
};
use crate::text::{sanitize, sanitize_line};

pub fn convert(
    items: &Vector<Arc<TimelineItem>>,
    own: &UserId,
    ignored: &HashSet<OwnedUserId>,
) -> Vec<Message> {
    let now = Local::now();
    let last = items.len().saturating_sub(1);
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let id = item.unique_id().0.clone();
            if let Some(event) = item.as_event() {
                return event_message(id, event, own, ignored, &now);
            }
            match item.as_virtual()? {
                // Keep a trailing `ReadMarker` too when every item after it was
                // hidden/ignored so the UI knows no visible unread messages follow it.
                VirtualTimelineItem::ReadMarker if index < last => Some(Message {
                    id,
                    kind: MsgKind::ReadMarker,
                    ..Message::default()
                }),
                // Day separators are derived from the messages' own dates.
                _ => None,
            }
        })
        .collect()
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

pub(super) fn display_name(profile: &TimelineDetails<Profile>, user: &UserId) -> String {
    match profile {
        TimelineDetails::Ready(Profile {
            display_name: Some(name),
            ..
        }) if !crate::text::is_blank(&sanitize_line(name)) => sanitize_line(name),
        _ => user.localpart().to_owned(),
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
fn event_message(
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
        sender: sender.clone(),
        sender_id: Some(event.sender().to_string()),
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

    match event.content() {
        TimelineItemContent::MsgLike(content) => {
            message.thread = super::threads::summary(content);
            message.reactions = content
                .reactions
                .iter()
                .filter_map(|(key, senders)| {
                    let count = senders
                        .keys()
                        .filter(|user| !ignored.contains(*user))
                        .count() as u32;
                    (count > 0).then(|| Reaction {
                        key: key.clone(),
                        count,
                        own: senders.contains_key(own),
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
                        display_name(&original.sender_profile, &original.sender),
                        original
                            .content
                            .as_message()
                            .map(|m| m.body().to_owned())
                            .unwrap_or_else(|| "a message".to_owned()),
                    ),
                    _ => (String::new(), "an earlier message".to_owned()),
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
                            let duration = c
                                .info
                                .as_ref()
                                .and_then(|info| info.duration)
                                .or_else(|| c.audio.as_ref().map(|audio| audio.duration));
                            let meta = audio_meta(c);
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
                }
                MsgLikeKind::UnableToDecrypt(encrypted) => {
                    let why = utd_why(encrypted);
                    message.kind = MsgKind::Undecryptable;
                    message.body = why.text().to_owned();
                    message.utd = Some(why);
                }
                MsgLikeKind::Sticker(sticker) => {
                    let content = sticker.content();
                    let mut image = image_info(
                        MediaSource::from(content.source.clone()),
                        &content.body,
                        None,
                        Some(&content.info),
                    );
                    image.sticker = true;
                    message.body = format!("[sticker] {}", image.name);
                    message.image = Some(image);
                }
                MsgLikeKind::Poll(poll) => {
                    convert_poll(&mut message, poll, event.is_editable(), own, ignored);
                }
                _ => message.body = "[unsupported message]".to_owned(),
            }
        }
        TimelineItemContent::MembershipChange(change) => {
            let who = change
                .display_name()
                .unwrap_or_else(|| change.user_id().localpart().to_owned());
            let what = match change.change() {
                Some(MembershipChange::Joined | MembershipChange::InvitationAccepted) => {
                    "joined".to_owned()
                }
                Some(MembershipChange::Left) => "left".to_owned(),
                Some(MembershipChange::Invited) => format!("was invited by {sender}"),
                Some(MembershipChange::InvitationRejected) => "declined the invitation".to_owned(),
                Some(MembershipChange::InvitationRevoked) => {
                    "had their invitation withdrawn".to_owned()
                }
                Some(MembershipChange::Kicked) => format!("was removed by {sender}"),
                Some(MembershipChange::Banned | MembershipChange::KickedAndBanned) => {
                    format!("was banned by {sender}")
                }
                Some(MembershipChange::Unbanned) => format!("was unbanned by {sender}"),
                Some(MembershipChange::Knocked) => "asked to join".to_owned(),
                _ => "changed membership".to_owned(),
            };
            system(&mut message, format!("{who} {what}"));
        }
        TimelineItemContent::ProfileChange(_) => {
            system(&mut message, format!("{sender} updated their profile"));
        }
        TimelineItemContent::OtherState(state) => {
            let what = state.content().event_type().to_string();
            let what = match what.as_str() {
                "m.room.name" => "the room name".to_owned(),
                "m.room.topic" => "the topic".to_owned(),
                "m.room.avatar" => "the room picture".to_owned(),
                "m.room.encryption" => "encryption (now on)".to_owned(),
                "m.room.create" => {
                    return Some(system_owned(message, format!("{sender} created the room")));
                }
                // Power levels, join rules and the like: noise in a chat view.
                _ => return None,
            };
            system(&mut message, format!("{sender} changed {what}"));
        }
        TimelineItemContent::CallInvite | TimelineItemContent::RtcNotification { .. } => {
            system(&mut message, format!("{sender} started a call"));
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
    votes_by_answer: &std::collections::HashMap<String, Vec<String>>,
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
    let mut voters: HashSet<String> = HashSet::new();
    let answers: Vec<crate::model::PollAnswer> = raw_answers
        .into_iter()
        .filter(|(id, _)| !id.is_empty() && id.len() <= ID_BYTES)
        .take(crate::modal::MAX_POLL_ANSWERS)
        .map(|(id, text)| {
            let mut votes = 0u32;
            let mut own_voted = false;
            if let Some(list) = votes_by_answer.get(&id) {
                for uid in list {
                    let is_ignored =
                        <&UserId>::try_from(uid.as_str()).is_ok_and(|u| ignored.contains(u));
                    if is_ignored {
                        continue;
                    }
                    votes = votes.saturating_add(1);
                    if uid == own.as_str() {
                        own_voted = true;
                    }
                    voters.insert(uid.clone());
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
        let bob = "@bob:example.org".to_owned();
        let eve = owned_user_id!("@eve:example.org");
        let ignored = HashSet::from([eve.clone()]);

        let mut votes = HashMap::new();
        votes.insert("a1".to_owned(), vec![own.to_string(), eve.to_string()]);
        votes.insert("a2".to_owned(), vec![bob.clone(), eve.to_string()]);

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
}
