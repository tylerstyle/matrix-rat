//! Turning SDK timeline items into what the UI shows.

use std::sync::Arc;

use chrono::{DateTime, Local, TimeZone};
use matrix_sdk::ruma::events::room::message::MessageType;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, UserId};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{
    EventSendState, EventTimelineItem, MembershipChange, MsgLikeKind, Profile, TimelineDetails,
    TimelineEventItemId, TimelineItem, TimelineItemContent, VirtualTimelineItem,
};

use crate::model::{ItemRef, Message, MsgKind, Reaction, SendState};
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

fn display_name(profile: &TimelineDetails<Profile>, user: &UserId) -> String {
    match profile {
        TimelineDetails::Ready(Profile {
            display_name: Some(name),
            ..
        }) if !sanitize_line(name).trim().is_empty() => sanitize_line(name),
        _ => user.localpart().to_owned(),
    }
}

fn local_time(timestamp: MilliSecondsSinceUnixEpoch) -> Option<DateTime<Local>> {
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

    match event.content() {
        TimelineItemContent::MsgLike(content) => {
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
            match &content.kind {
                MsgLikeKind::Message(msg) => {
                    message.edited = msg.is_edited();
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
                        MessageType::Image(c) => format!("🖼  {}", c.filename()),
                        MessageType::File(c) => format!("📎 {}", c.filename()),
                        MessageType::Video(c) => format!("🎞  {}", c.filename()),
                        MessageType::Audio(c) => format!("🔊 {}", c.filename()),
                        MessageType::Location(c) => format!("📍 {}", c.body),
                        other => other.body().to_owned(),
                    };
                }
                MsgLikeKind::Redacted => {
                    message.kind = MsgKind::Redacted;
                    message.body = "Message deleted".to_owned();
                }
                MsgLikeKind::UnableToDecrypt(_) => {
                    message.kind = MsgKind::Undecryptable;
                    message.body = "Unable to decrypt (waiting for keys)".to_owned();
                }
                MsgLikeKind::Sticker(_) => message.body = "[sticker]".to_owned(),
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
