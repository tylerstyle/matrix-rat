//! Turning SDK timeline items into what the UI shows.

use std::sync::Arc;

use chrono::{DateTime, Local, TimeZone};
use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk::ruma::events::room::{ImageInfo as RumaImageInfo, MediaSource};
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, UserId};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    EventSendState, EventTimelineItem, MembershipChange, MsgLikeKind, Profile, TimelineDetails,
    TimelineEventItemId, TimelineItem, TimelineItemContent, VirtualTimelineItem,
};

use crate::model::{
    Attachment, ImageInfo, ImageSource, ItemRef, MediaRef, Message, MsgKind, Reaction, SendState,
    Shield,
};
use crate::text::{sanitize, sanitize_line};

pub fn convert(items: &Vector<Arc<TimelineItem>>, own: &UserId) -> Vec<Message> {
    let now = Local::now();
    let last = items.len().saturating_sub(1);
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let id = item.unique_id().0.clone();
            if let Some(event) = item.as_event() {
                return event_message(id, event, own, &now);
            }
            match item.as_virtual()? {
                // Nothing is new below a marker that sits at the very end.
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

/// One line saying what a message is, for the room list.
pub(super) fn preview(content: &TimelineItemContent) -> String {
    let text = match content {
        TimelineItemContent::MsgLike(content) => match &content.kind {
            MsgLikeKind::Message(message) => match message.msgtype() {
                MessageType::Image(_) => "sent a picture".to_owned(),
                MessageType::Video(_) => "sent a video".to_owned(),
                MessageType::Audio(_) => "sent an audio message".to_owned(),
                MessageType::File(file) => format!("📎 {}", file.filename()),
                other => other.body().to_owned(),
            },
            MsgLikeKind::Sticker(_) => "sent a sticker".to_owned(),
            MsgLikeKind::Poll(_) => "started a poll".to_owned(),
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
    now: &DateTime<Local>,
) -> Option<Message> {
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
        own: event.is_own(),
        sender: sender.clone(),
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
                .map(|(key, senders)| Reaction {
                    key: key.clone(),
                    count: senders.len() as u32,
                    own: senders.contains_key(own),
                })
                .collect();
            message.reply_to = content
                .in_reply_to
                .as_ref()
                .map(|reply| match &reply.event {
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
                            attach(
                                &mut message,
                                "📎",
                                &c.source,
                                c.filename(),
                                c.caption(),
                                size,
                            )
                        }
                        MessageType::Video(c) => {
                            let size = c.info.as_ref().and_then(|info| info.size);
                            attach(
                                &mut message,
                                "🎞 ",
                                &c.source,
                                c.filename(),
                                c.caption(),
                                size,
                            )
                        }
                        MessageType::Audio(c) => {
                            let size = c.info.as_ref().and_then(|info| info.size);
                            attach(
                                &mut message,
                                "🔊",
                                &c.source,
                                c.filename(),
                                c.caption(),
                                size,
                            )
                        }
                        MessageType::Location(c) => format!("📍 {}", c.body),
                        other => other.body().to_owned(),
                    };
                }
                MsgLikeKind::Redacted => {
                    // A deleted message has no content to doubt.
                    message.shield = None;
                    message.kind = MsgKind::Redacted;
                    message.body = "Message deleted".to_owned();
                }
                MsgLikeKind::UnableToDecrypt(_) => {
                    message.kind = MsgKind::Undecryptable;
                    message.body = "Unable to decrypt (waiting for keys)".to_owned();
                }
                MsgLikeKind::Sticker(sticker) => {
                    let content = sticker.content();
                    let image = image_info(
                        MediaSource::from(content.source.clone()),
                        &content.body,
                        None,
                        Some(&content.info),
                    );
                    message.body = format!("[sticker] {}", image.name);
                    message.image = Some(image);
                }
                MsgLikeKind::Poll(_) => message.body = "[poll]".to_owned(),
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

/// Marks a message as carrying a file and returns the line that stands for it.
fn attach(
    message: &mut Message,
    icon: &str,
    source: &MediaSource,
    filename: &str,
    caption: Option<&str>,
    size: Option<matrix_sdk::ruma::UInt>,
) -> String {
    let key = match source {
        MediaSource::Plain(uri) => uri.to_string(),
        MediaSource::Encrypted(file) => file.url.to_string(),
    };
    let name = sanitize_line(filename);
    let name = if name.trim().is_empty() {
        "attachment".to_owned()
    } else {
        name
    };
    let size = size.map(u64::from);
    let mut text = match size {
        Some(size) => format!("{icon} {name} · {}", crate::images::human_size(size)),
        None => format!("{icon} {name}"),
    };
    if let Some(caption) = caption.map(sanitize).filter(|c| !c.trim().is_empty()) {
        text.push('\n');
        text.push_str(&caption);
    }
    message.file = Some(Attachment {
        key: sanitize_line(&key),
        name,
        size,
        source: Box::new(source.clone()),
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
    let key = match &source {
        MediaSource::Plain(uri) => uri.to_string(),
        MediaSource::Encrypted(file) => file.url.to_string(),
    };
    let name = sanitize_line(filename);
    let dimensions = info.and_then(|info| {
        let width = u32::try_from(u64::from(info.width?)).ok()?;
        let height = u32::try_from(u64::from(info.height?)).ok()?;
        (width > 0 && height > 0).then_some((width, height))
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

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
}
