//! Worker-side implementation for pinned messages and message forwarding.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Local, TimeZone};
use futures::StreamExt;
use matrix_sdk::attachment::{
    AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo,
    Thumbnail,
};
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::ruma::events::relation::RelationType;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::room::message::{
    AudioMessageEventContent, FileMessageEventContent, FormattedBody, ImageMessageEventContent,
    MessageType, RoomMessageEventContent, SyncRoomMessageEvent, TextMessageEventContent,
    VideoMessageEventContent,
};
use matrix_sdk::ruma::events::sticker::{StickerEventContent, SyncStickerEvent};
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId};
use matrix_sdk::{Client, Room, RoomState};

use super::convert::day_label;
use super::manage;
use super::safety::IgnoredSet;
use super::search;
use crate::model::PinnedMessage;
use crate::text::{sanitize_line, truncate};

/// Upper bound on cached remote-fetched pinned events so repeated opens avoid `/event` requests.
const PINNED_CACHE_MAX: usize = 128;
/// Cap how many pinned events are fetched in one request.
const MAX_PINNED_FETCH: usize = 50;
/// Bounded concurrency for `/event` lookups of unloaded pins.
const PINNED_CONCURRENCY: usize = 4;
const PINNED_SNIPPET_MAX: usize = 120;
const FORWARD_TIMEOUT: Duration = Duration::from_secs(60);
/// What stands for an event that could not be loaded.
const UNAVAILABLE: &str = "[message unavailable]";
/// What stands for an event that has no text to show.
const UNSUPPORTED: &str = "[unsupported pinned item]";

#[derive(Clone)]
struct CachedPinned {
    sender_id: Option<String>,
    sender: String,
    day: String,
    time: String,
    snippet: String,
    redacted: bool,
    undecryptable: bool,
    /// An event of a kind that has no text; `snippet` says so in the pinned
    /// list's words.
    no_text: bool,
}

/// Bounded FIFO cache for pinned events loaded via `/event`.
#[derive(Default)]
pub(super) struct PinnedCache {
    entries: HashMap<(String, String), CachedPinned>,
    order: VecDeque<(String, String)>,
}

impl PinnedCache {
    fn get(&self, room_id: &str, event_id: &str) -> Option<&CachedPinned> {
        self.entries.get(&(room_id.to_owned(), event_id.to_owned()))
    }

    fn insert(&mut self, room_id: String, event_id: String, entry: CachedPinned) {
        let key = (room_id, event_id);
        if let Some(slot) = self.entries.get_mut(&key) {
            *slot = entry;
            return;
        }
        if self.order.len() >= PINNED_CACHE_MAX
            && let Some(oldest) = self.order.pop_front()
        {
            self.entries.remove(&oldest);
        }
        self.order.push_back(key.clone());
        self.entries.insert(key, entry);
    }

    pub(super) fn invalidate_room_event(&mut self, room_id: &str, event_id: &str) {
        self.entries
            .remove(&(room_id.to_owned(), event_id.to_owned()));
    }
}

pub(super) async fn pin(
    room: &Room,
    event_id: &str,
    pinned: bool,
    cache: &Arc<Mutex<PinnedCache>>,
) -> Result<Vec<String>, String> {
    let eid = EventId::parse(event_id).map_err(|_| "That is not a valid message ID.".to_owned())?;
    if pinned {
        room.pin_event(&eid)
            .await
            .map_err(|err| manage::failed("pin that message", &err))?;
    } else {
        room.unpin_event(&eid)
            .await
            .map_err(|err| manage::failed("unpin that message", &err))?;
    }
    cache
        .lock()
        .expect("pinned cache poisoned")
        .invalidate_room_event(room.room_id().as_str(), event_id);
    let existing = match room.pinned_event_ids() {
        Some(ids) => ids,
        None => room
            .load_pinned_events()
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
    };
    let mut ids: Vec<String> = existing.into_iter().map(|id| id.to_string()).collect();
    if pinned {
        if !ids.iter().any(|id| id == event_id) {
            ids.push(event_id.to_owned());
        }
    } else {
        ids.retain(|id| id != event_id);
    }
    Ok(ids)
}

pub(super) async fn fetch_pinned(
    room: &Room,
    cache: &Arc<Mutex<PinnedCache>>,
    ignored: &IgnoredSet,
) -> (Vec<PinnedMessage>, Option<String>) {
    let ids = match room.pinned_event_ids() {
        Some(ids) => ids,
        None => match room.load_pinned_events().await {
            Ok(Some(ids)) => ids,
            Ok(None) => Vec::new(),
            Err(err) => {
                return (
                    Vec::new(),
                    Some(manage::failed("load pinned messages", &err)),
                );
            }
        },
    };
    let room_id = room.room_id().to_string();
    let own_user = room.own_user_id().as_str().to_owned();
    let total = ids.len();
    // Show newest pinned events first while keeping fetch cost bounded.
    let wanted: Vec<OwnedEventId> = ids.into_iter().rev().take(MAX_PINNED_FETCH).collect();
    let mut name_cache: HashMap<String, String> = HashMap::new();
    let results: Vec<PinnedMessage> = futures::stream::iter(wanted)
        .map(|eid| {
            let room = room.clone();
            let room_id = room_id.clone();
            let cache = cache.clone();
            async move { load_one_pinned(&room, &room_id, &eid, &cache).await }
        })
        .buffered(PINNED_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|(eid, mut cached)| {
            // Check local member display name synchronously/cached in-memory if needed.
            if let Some(uid) = &cached.sender_id
                && let Some(cached_name) = name_cache.get(uid)
            {
                cached.sender.clone_from(cached_name);
            } else if let Some(uid) = &cached.sender_id {
                name_cache.insert(uid.clone(), cached.sender.clone());
            }
            let is_ignored = cached
                .sender_id
                .as_deref()
                .is_some_and(|uid| uid != own_user && ignored.contains(uid));
            if is_ignored {
                PinnedMessage {
                    event_id: eid,
                    sender: "Ignored user".to_owned(),
                    sender_id: None,
                    day: cached.day,
                    time: cached.time,
                    snippet: "Message from an ignored user".to_owned(),
                    redacted: false,
                    undecryptable: false,
                    ignored: true,
                }
            } else {
                PinnedMessage {
                    event_id: eid,
                    sender: cached.sender,
                    sender_id: cached.sender_id,
                    day: cached.day,
                    time: cached.time,
                    snippet: cached.snippet,
                    redacted: cached.redacted,
                    undecryptable: cached.undecryptable,
                    ignored: false,
                }
            }
        })
        .collect();
    let note = (total > MAX_PINNED_FETCH)
        .then(|| format!("Showing newest {MAX_PINNED_FETCH} of {total} pinned messages."));
    (results, note)
}

/// What an event is, for the saved messages list.
pub(super) enum EventSummary {
    Found {
        sender: String,
        sender_id: Option<String>,
        kind: SummaryKind,
    },
    /// It could not be loaded (or its id is none).
    Unavailable,
}

pub(super) enum SummaryKind {
    /// Its text, one line, cut as the pinned list cuts it.
    Text(String),
    /// A kind of event that has no text to show.
    NoText,
    Redacted,
    Undecryptable,
}

/// One event summarised as the pinned list would show it.
pub(super) async fn summarize_event(
    room: &Room,
    event_id: &str,
    cache: &Arc<Mutex<PinnedCache>>,
) -> EventSummary {
    let Ok(eid) = EventId::parse(event_id) else {
        return EventSummary::Unavailable;
    };
    let Some(found) = load_event(room, room.room_id().as_str(), &eid, cache).await else {
        return EventSummary::Unavailable;
    };
    EventSummary::Found {
        sender: found.sender,
        sender_id: found.sender_id,
        kind: if found.redacted {
            SummaryKind::Redacted
        } else if found.undecryptable {
            SummaryKind::Undecryptable
        } else if found.no_text {
            SummaryKind::NoText
        } else {
            SummaryKind::Text(found.snippet)
        },
    }
}

/// One pinned event; what could not be loaded gets a line that says so.
async fn load_one_pinned(
    room: &Room,
    room_id: &str,
    eid: &EventId,
    cache: &Arc<Mutex<PinnedCache>>,
) -> (String, CachedPinned) {
    let found = load_event(room, room_id, eid, cache).await;
    let found = found.unwrap_or_else(|| CachedPinned {
        sender_id: None,
        sender: String::new(),
        day: String::new(),
        time: String::new(),
        snippet: UNAVAILABLE.to_owned(),
        redacted: false,
        undecryptable: false,
        no_text: false,
    });
    (eid.to_string(), found)
}

/// Loads one event: from the event cache, else from what was loaded before, else
/// from the server. `None` if it is not to be had.
async fn load_event(
    room: &Room,
    room_id: &str,
    eid: &EventId,
    cache: &Arc<Mutex<PinnedCache>>,
) -> Option<CachedPinned> {
    let eid_str = eid.to_string();
    // Local EventCache reflects redactions and edits without network traffic.
    if let Ok((event_cache, _drops)) = room.event_cache().await
        && let Ok(Some((ev, mut edits))) = event_cache
            .find_event_with_relations(eid, Some(vec![RelationType::Replacement]))
            .await
    {
        let ev = search::heal_utd_event(room, &event_cache, ev).await;
        for edit in &mut edits {
            if edit.kind.is_utd() {
                *edit = search::heal_utd_event(room, &event_cache, edit.clone()).await;
            }
        }
        let parsed = build_cached_pinned(room, &ev, &edits).await;
        if !parsed.undecryptable {
            cache.lock().expect("pinned cache poisoned").insert(
                room_id.to_owned(),
                eid_str,
                parsed.clone(),
            );
        }
        return Some(parsed);
    }

    if let Some(hit) = cache
        .lock()
        .expect("pinned cache poisoned")
        .get(room_id, &eid_str)
        .cloned()
    {
        return Some(hit);
    }

    let ev = room.load_or_fetch_event(eid, None).await.ok()?;
    let parsed = build_cached_pinned(room, &ev, &[]).await;
    if !parsed.undecryptable {
        cache.lock().expect("pinned cache poisoned").insert(
            room_id.to_owned(),
            eid_str,
            parsed.clone(),
        );
    }
    Some(parsed)
}

async fn build_cached_pinned(
    room: &Room,
    event: &TimelineEvent,
    relations: &[TimelineEvent],
) -> CachedPinned {
    let (day, time) = event.timestamp.map(format_day_time).unwrap_or_default();
    let sender_owned = event.sender();
    let sender_id = sender_owned.as_deref().map(|u| u.as_str().to_owned());
    let sender = match sender_owned.as_deref() {
        Some(uid) => sender_display_name(room, uid).await,
        None => String::new(),
    };

    if event.kind.is_utd() {
        return CachedPinned {
            sender_id,
            sender,
            day,
            time,
            snippet: "Encrypted message (key unavailable)".to_owned(),
            redacted: false,
            undecryptable: true,
            no_text: false,
        };
    }

    if is_event_redacted(event) {
        return CachedPinned {
            sender_id,
            sender,
            day,
            time,
            snippet: "[message deleted]".to_owned(),
            redacted: true,
            undecryptable: false,
            no_text: false,
        };
    }

    let text = search::extract_event_text(event, relations);
    let no_text = text.is_none();
    let snippet = match text {
        Some(text) => {
            let clean = sanitize_line(&text);
            if clean.is_empty() {
                "[message]".to_owned()
            } else {
                truncate(clean.trim(), PINNED_SNIPPET_MAX)
            }
        }
        None => UNSUPPORTED.to_owned(),
    };

    CachedPinned {
        sender_id,
        sender,
        day,
        time,
        snippet,
        redacted: false,
        undecryptable: false,
        no_text,
    }
}

pub(super) fn format_day_time(ts: MilliSecondsSinceUnixEpoch) -> (String, String) {
    let Ok(millis) = i64::try_from(u64::from(ts.get())) else {
        return (String::new(), String::new());
    };
    let Some(when) = Local.timestamp_millis_opt(millis).single() else {
        return (String::new(), String::new());
    };
    let now = Local::now();
    (day_label(&when, &now), when.format("%H:%M").to_string())
}

async fn sender_display_name(room: &Room, user_id: &matrix_sdk::ruma::UserId) -> String {
    let mut cache = HashMap::new();
    search::cached_sender_name(room, user_id, &mut cache).await
}

pub(super) fn is_event_redacted(event: &TimelineEvent) -> bool {
    if let Ok(raw) = serde_json::from_str::<serde_json::Value>(event.raw().json().get())
        && raw
            .get("unsigned")
            .and_then(|u| u.get("redacted_because"))
            .is_some()
    {
        return true;
    }
    matches!(
        event.raw().deserialize(),
        Ok(AnySyncTimelineEvent::MessageLike(
            AnySyncMessageLikeEvent::RoomMessage(SyncRoomMessageEvent::Redacted(_))
        ))
    )
}

struct MediaForward {
    source: MediaSource,
    thumbnail_source: Option<MediaSource>,
    thumbnail_size: Option<(matrix_sdk::ruma::UInt, matrix_sdk::ruma::UInt, String)>,
    filename: String,
    body: String,
    formatted: Option<FormattedBody>,
    mimetype: Option<String>,
    info: AttachmentInfo,
}

fn source_is_encrypted(source: &MediaSource) -> bool {
    matches!(source, MediaSource::Encrypted(_))
}

fn extract_media_forward(msgtype: &MessageType) -> Option<MediaForward> {
    #[expect(clippy::wildcard_enum_match_arm, reason = "foreign ruma MessageType")]
    match msgtype {
        MessageType::Image(c) => {
            let thumb_src = c.info.as_ref().and_then(|i| i.thumbnail_source.clone());
            let thumb_dims = c.info.as_ref().and_then(|i| {
                let ti = i.thumbnail_info.as_deref()?;
                Some((ti.width?, ti.height?, ti.mimetype.clone()?))
            });
            let info =
                AttachmentInfo::Image(c.info.as_deref().map_or_else(BaseImageInfo::default, |i| {
                    BaseImageInfo {
                        height: i.height,
                        width: i.width,
                        size: i.size,
                        blurhash: i.blurhash.clone(),
                        is_animated: i.is_animated,
                    }
                }));
            Some(MediaForward {
                source: c.source.clone(),
                thumbnail_source: thumb_src,
                thumbnail_size: thumb_dims,
                filename: c.filename().to_owned(),
                body: c.body.clone(),
                formatted: c.formatted.clone(),
                mimetype: c.info.as_deref().and_then(|i| i.mimetype.clone()),
                info,
            })
        }
        MessageType::File(c) => {
            let thumb_src = c.info.as_ref().and_then(|i| i.thumbnail_source.clone());
            let thumb_dims = c.info.as_ref().and_then(|i| {
                let ti = i.thumbnail_info.as_deref()?;
                Some((ti.width?, ti.height?, ti.mimetype.clone()?))
            });
            let info = AttachmentInfo::File(
                c.info
                    .as_deref()
                    .map_or_else(BaseFileInfo::default, |i| BaseFileInfo { size: i.size }),
            );
            Some(MediaForward {
                source: c.source.clone(),
                thumbnail_source: thumb_src,
                thumbnail_size: thumb_dims,
                filename: c.filename().to_owned(),
                body: c.body.clone(),
                formatted: c.formatted.clone(),
                mimetype: c.info.as_deref().and_then(|i| i.mimetype.clone()),
                info,
            })
        }
        MessageType::Video(c) => {
            let thumb_src = c.info.as_ref().and_then(|i| i.thumbnail_source.clone());
            let thumb_dims = c.info.as_ref().and_then(|i| {
                let ti = i.thumbnail_info.as_deref()?;
                Some((ti.width?, ti.height?, ti.mimetype.clone()?))
            });
            let info =
                AttachmentInfo::Video(c.info.as_deref().map_or_else(BaseVideoInfo::default, |i| {
                    BaseVideoInfo {
                        duration: i.duration,
                        height: i.height,
                        width: i.width,
                        size: i.size,
                        blurhash: i.blurhash.clone(),
                    }
                }));
            Some(MediaForward {
                source: c.source.clone(),
                thumbnail_source: thumb_src,
                thumbnail_size: thumb_dims,
                filename: c.filename().to_owned(),
                body: c.body.clone(),
                formatted: c.formatted.clone(),
                mimetype: c.info.as_deref().and_then(|i| i.mimetype.clone()),
                info,
            })
        }
        MessageType::Audio(c) => {
            let info =
                AttachmentInfo::Audio(c.info.as_deref().map_or_else(BaseAudioInfo::default, |i| {
                    BaseAudioInfo {
                        duration: i.duration,
                        size: i.size,
                        waveform: None,
                    }
                }));
            Some(MediaForward {
                source: c.source.clone(),
                thumbnail_source: None,
                thumbnail_size: None,
                filename: c.filename().to_owned(),
                body: c.body.clone(),
                formatted: c.formatted.clone(),
                mimetype: c.info.as_deref().and_then(|i| i.mimetype.clone()),
                info,
            })
        }
        _ => None,
    }
}

/// Forwards a message from `from_room` into `to_room` as a standalone copy (`relates_to = None`).
/// Reuses existing MXC media URLs/blobs whenever encryption domain allows, and downloads &
/// re-uploads when crossing between encrypted and unencrypted rooms.
pub(super) async fn forward_message(
    client: &Client,
    from_room: &Room,
    to_room: &Room,
    event_id: &str,
) -> Result<(), String> {
    if to_room.state() != RoomState::Joined {
        return Err("Join the target room before forwarding to it.".to_owned());
    }
    let eid = EventId::parse(event_id).map_err(|_| "That is not a valid message ID.".to_owned())?;
    let (event, edits) = search::load_event_with_replacements(from_room, &eid)
        .await
        .ok_or_else(|| "Could not find the message to forward.".to_owned())?;

    if event.kind.is_utd() {
        return Err("Cannot forward an encrypted message that cannot be decrypted.".to_owned());
    }
    if is_event_redacted(&event) {
        return Err("Cannot forward a deleted message.".to_owned());
    }

    let to_encrypted = to_room
        .latest_encryption_state()
        .await
        .map(|s| s.is_encrypted())
        .unwrap_or(false);

    match event.raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg))) => {
            let SyncRoomMessageEvent::Original(orig) = msg else {
                return Err("Cannot forward a deleted message.".to_owned());
            };
            let replaced = search::latest_replacement(&edits, &orig.event_id, &orig.sender)
                .or_else(|| {
                    search::bundled_replacement(&orig.unsigned.relations.replace, &orig.sender)
                });
            let msgtype = replaced.unwrap_or_else(|| orig.content.msgtype.clone());

            if let Some(media) = extract_media_forward(&msgtype) {
                let src_encrypted = source_is_encrypted(&media.source);
                let thumb_matches = media
                    .thumbnail_source
                    .as_ref()
                    .is_none_or(|t| source_is_encrypted(t) == to_encrypted);
                if src_encrypted == to_encrypted && thumb_matches {
                    // Same encryption state: send standalone copy reusing the existing MXC blob.
                    to_room
                        .send(RoomMessageEventContent::new(msgtype))
                        .await
                        .map_err(|err| manage::failed("forward that message", &err))?;
                    return Ok(());
                }
                // Cross-boundary forwarding: download decrypted bytes and re-upload appropriately.
                forward_media_reupload(client, to_room, media).await
            } else {
                // Standalone copy: `RoomMessageEventContent::new` clears `relates_to` and `mentions`.
                to_room
                    .send(RoomMessageEventContent::new(msgtype))
                    .await
                    .map_err(|err| manage::failed("forward that message", &err))?;
                Ok(())
            }
        }
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Sticker(sticker))) => {
            let SyncStickerEvent::Original(orig) = sticker else {
                return Err("Cannot forward a deleted sticker.".to_owned());
            };
            #[expect(clippy::wildcard_enum_match_arm, reason = "foreign ruma MediaSource")]
            match MediaSource::from(orig.content.source.clone()) {
                MediaSource::Plain(url) if !to_encrypted => {
                    let content = StickerEventContent::new(
                        orig.content.body.clone(),
                        orig.content.info.clone(),
                        url,
                    );
                    to_room
                        .send(content)
                        .await
                        .map_err(|err| manage::failed("forward that sticker", &err))?;
                    Ok(())
                }
                source => {
                    let media = MediaForward {
                        source,
                        thumbnail_source: orig.content.info.thumbnail_source.clone(),
                        thumbnail_size: orig
                            .content
                            .info
                            .thumbnail_info
                            .as_deref()
                            .and_then(|ti| Some((ti.width?, ti.height?, ti.mimetype.clone()?))),
                        filename: orig.content.body.clone(),
                        body: orig.content.body.clone(),
                        formatted: None,
                        mimetype: orig.content.info.mimetype.clone(),
                        info: AttachmentInfo::Image(BaseImageInfo {
                            height: orig.content.info.height,
                            width: orig.content.info.width,
                            size: orig.content.info.size,
                            blurhash: None,
                            is_animated: None,
                        }),
                    };
                    forward_media_reupload(client, to_room, media).await
                }
            }
        }
        _ => Err("This message type cannot be forwarded.".to_owned()),
    }
}

async fn forward_media_reupload(
    client: &Client,
    to_room: &Room,
    media: MediaForward,
) -> Result<(), String> {
    let request = MediaRequestParameters {
        source: media.source,
        format: MediaFormat::File,
    };
    let data = tokio::time::timeout(
        FORWARD_TIMEOUT,
        client.media().get_media_content(&request, false),
    )
    .await
    .map_err(|_| "Could not forward media: downloading took too long.".to_owned())?
    .map_err(|err| manage::failed("download the forwarded media", &err))?;

    let thumbnail = match (media.thumbnail_source, media.thumbnail_size) {
        (Some(thumb_source), Some((width, height, thumb_mime))) => {
            let thumb_req = MediaRequestParameters {
                source: thumb_source,
                format: MediaFormat::File,
            };
            if let Ok(Ok(thumb_bytes)) = tokio::time::timeout(
                FORWARD_TIMEOUT,
                client.media().get_media_content(&thumb_req, false),
            )
            .await
                && let Ok(content_type) = thumb_mime.parse::<mime::Mime>()
                && let Ok(size) = matrix_sdk::ruma::UInt::try_from(thumb_bytes.len())
            {
                Some(Thumbnail {
                    data: thumb_bytes,
                    content_type,
                    height,
                    width,
                    size,
                })
            } else {
                None
            }
        }
        _ => None,
    };

    let content_type = media
        .mimetype
        .as_deref()
        .and_then(|s| s.parse::<mime::Mime>().ok())
        .unwrap_or(mime::APPLICATION_OCTET_STREAM);
    let caption = if media.body != media.filename || media.formatted.is_some() {
        Some(match media.formatted {
            Some(fmt) => {
                let mut t = TextMessageEventContent::plain(media.body);
                t.formatted = Some(fmt);
                t
            }
            None => TextMessageEventContent::plain(media.body),
        })
    } else {
        None
    };
    let config = AttachmentConfig::new()
        .info(media.info)
        .thumbnail(thumbnail)
        .caption(caption);
    tokio::time::timeout(
        FORWARD_TIMEOUT,
        to_room.send_attachment(&media.filename, &content_type, data, config),
    )
    .await
    .map_err(|_| "Could not forward media: uploading took too long.".to_owned())?
    .map_err(|err| manage::failed("forward that media", &err))?;
    Ok(())
}

/// Ensures `MediaSource` construction compiles cleanly when building `RoomMessageEventContent`
/// in unit tests or future extensions.
#[allow(dead_code)]
fn _keep_imports_used(
    _: (
        ImageMessageEventContent,
        FileMessageEventContent,
        VideoMessageEventContent,
        AudioMessageEventContent,
    ),
) {
}

#[cfg(test)]
mod tests {
    use super::super::{
        Change, ManageCmd, ManageEvent, MatrixCmd, MatrixEvent, MatrixHandle, MemberRole,
        RoomDetails, spawn,
    };
    use crate::format;
    use crate::model::{Message, RoomPower};
    use crate::paths::Paths;
    use futures::FutureExt;
    use matrix_sdk::Client;
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::RoomId;
    use std::panic::AssertUnwindSafe;
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn hs() -> String {
        std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned())
    }

    async fn login_worker(
        tag: &str,
        homeserver: &str,
        username: &str,
        password: &str,
    ) -> (
        PathBuf,
        MatrixHandle,
        UnboundedReceiver<MatrixEvent>,
        String,
    ) {
        let root =
            std::env::temp_dir().join(format!("matrix-rat-mod-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver: homeserver.to_owned(),
            username: username.to_owned(),
            password: password.to_owned().into(),
        });
        let user_id = loop {
            match tokio::time::timeout(Duration::from_secs(60), events.recv())
                .await
                .expect("timed out logging in")
                .expect("worker stopped")
            {
                MatrixEvent::LoggedIn { user_id } => break user_id,
                MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                _ => {}
            }
        };
        (root, handle, events, user_id)
    }

    async fn login_bob(homeserver: &str) -> (Client, String) {
        let bob = Client::builder()
            .homeserver_url(homeserver)
            .build()
            .await
            .unwrap();
        bob.matrix_auth()
            .login_username("bob", "bobpass")
            .initial_device_display_name("matrix-rat moderation test")
            .send()
            .await
            .unwrap();
        let id = bob.user_id().unwrap().to_string();
        (bob, id)
    }

    async fn create_room(
        handle: &MatrixHandle,
        events: &mut UnboundedReceiver<MatrixEvent>,
        name: &str,
        encrypted: bool,
    ) -> String {
        handle.send(MatrixCmd::Manage(ManageCmd::Create {
            request: 0,
            name: name.to_owned(),
            topic: None,
            alias: None,
            encrypted,
        }));
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(ManageEvent::Opened { result, .. }) => {
                        return result.expect("Create room failed");
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for Created room")
    }

    async fn wait_done(
        events: &mut UnboundedReceiver<MatrixEvent>,
        expected: Change,
    ) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(ManageEvent::Done { change, result, .. })
                        if change == expected =>
                    {
                        return result;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for ManageEvent::Done")
    }

    async fn wait_info(
        handle: &MatrixHandle,
        events: &mut UnboundedReceiver<MatrixEvent>,
        room_id: &str,
        phase: &str,
        pred: impl Fn(&RoomDetails) -> bool,
    ) -> RoomDetails {
        handle.send(MatrixCmd::Manage(ManageCmd::Info {
            room_id: room_id.to_owned(),
        }));
        let mut last: Option<RoomDetails> = None;
        let res = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(ManageEvent::Info {
                        room_id: id,
                        result: Ok(details),
                    }) if id == room_id => {
                        if pred(&details) {
                            return *details;
                        }
                        last = Some(*details);
                    }
                    MatrixEvent::Manage(ManageEvent::Info {
                        room_id: id,
                        result: Err(err),
                    }) if id == room_id => panic!("{phase}: ManageEvent::Info failed: {err}"),
                    MatrixEvent::Error(err) => panic!("{phase}: {err}"),
                    _ => {}
                }
            }
        })
        .await;
        match res {
            Ok(details) => details,
            Err(_) => {
                let diag = last.map(|d| {
                    let members: Vec<_> = d
                        .members
                        .iter()
                        .map(|m| {
                            (
                                m.own,
                                m.role,
                                m.power_level,
                                m.invited,
                                m.banned,
                                m.bannable,
                                m.unbannable,
                            )
                        })
                        .collect();
                    format!(
                        "joined={}, invited={}, loading={}, err={:?}, members={members:?}",
                        d.joined, d.invited, d.members_loading, d.members_error
                    )
                });
                panic!("{phase}: timed out waiting for ManageEvent::Info; last={diag:?}");
            }
        }
    }

    async fn wait_power(
        events: &mut UnboundedReceiver<MatrixEvent>,
        room_id: &str,
        pred: impl Fn(&RoomPower) -> bool,
    ) -> RoomPower {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::RoomPower { room_id: id, power }
                        if id == room_id && pred(&power) =>
                    {
                        return power;
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for RoomPower")
    }

    async fn wait_timeline_msg(
        events: &mut UnboundedReceiver<MatrixEvent>,
        room_id: &str,
        pred: impl Fn(&[Message]) -> Option<Message>,
    ) -> Message {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Timeline {
                        room_id: id,
                        messages,
                        ..
                    } if id == room_id => {
                        if let Some(found) = pred(&messages) {
                            return found;
                        }
                    }
                    MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for Timeline message")
    }

    async fn invite_and_join_bob(
        alice: &MatrixHandle,
        alice_events: &mut UnboundedReceiver<MatrixEvent>,
        bob: &Client,
        room_id: &str,
        bob_id: &str,
    ) {
        alice.send(MatrixCmd::Manage(ManageCmd::Invite {
            room_id: room_id.to_owned(),
            user_id: bob_id.to_owned(),
        }));
        wait_done(alice_events, Change::Invite).await.unwrap();
        let rid = <&RoomId>::try_from(room_id).unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                bob.sync_once(SyncSettings::default()).await.unwrap();
                if bob.join_room_by_id(rid).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .expect("timed out joining room as bob");
    }

    async fn cleanup_rooms(
        workers: Vec<(
            &MatrixHandle,
            &mut UnboundedReceiver<MatrixEvent>,
            &[String],
        )>,
        bob: Option<&Client>,
        bob_room_ids: &[String],
        roots: Vec<PathBuf>,
    ) {
        for (handle, events, leave_ids) in workers {
            handle.send(MatrixCmd::Manage(ManageCmd::StopInfo));
            for id in leave_ids {
                handle.send(MatrixCmd::Manage(ManageCmd::Leave {
                    room_id: id.clone(),
                }));
                let _ = tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        match events.recv().await {
                            Some(MatrixEvent::Manage(ManageEvent::Left { room_id, .. }))
                                if room_id == *id =>
                            {
                                break;
                            }
                            None => break,
                            _ => {}
                        }
                    }
                })
                .await;
            }
            handle.send(MatrixCmd::Logout);
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match events.recv().await {
                        Some(MatrixEvent::NeedLogin { .. }) | None => break,
                        _ => {}
                    }
                }
            })
            .await;
            handle.send(MatrixCmd::Shutdown);
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                while events.recv().await.is_some() {}
            })
            .await;
        }
        if let Some(bob) = bob {
            let _ = bob.sync_once(SyncSettings::default()).await;
            for id in bob_room_ids {
                if let Ok(rid) = <&RoomId>::try_from(id.as_str())
                    && let Some(room) = bob.get_room(rid)
                {
                    let _ = room.leave().await;
                    let _ = room.forget().await;
                }
            }
            let _ = tokio::time::timeout(Duration::from_secs(10), bob.matrix_auth().logout()).await;
        }
        for root in roots {
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn server_unprivileged_member_cannot_ban_or_pin() {
        let hs = hs();
        let alice_user = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let alice_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let (root_a, h_a, mut ev_a, alice_id) =
            login_worker("unpriv-a", &hs, &alice_user, &alice_pass).await;
        let (root_b, h_b, mut ev_b, bob_id) = login_worker("unpriv-b", &hs, "bob", "bobpass").await;

        let room_id = create_room(&h_a, &mut ev_a, "matrix-rat mod unpriv test", false).await;
        let cleanup_ids = vec![room_id.clone()];

        let outcome = AssertUnwindSafe(async {
            h_a.send(MatrixCmd::Manage(ManageCmd::Invite {
                room_id: room_id.clone(),
                user_id: bob_id.clone(),
            }));
            wait_done(&mut ev_a, Change::Invite).await.unwrap();

            // Bob waits for the invite and joins via his worker.
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match ev_b.recv().await.expect("worker stopped") {
                        MatrixEvent::Rooms(rooms) if rooms.iter().any(|r| r.id == room_id) => {
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("bob invite timed out");

            h_b.send(MatrixCmd::Manage(ManageCmd::Join {
                request: 0,
                target: room_id.clone(),
                via: Vec::new(),
            }));
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match ev_b.recv().await.expect("worker stopped") {
                        MatrixEvent::Rooms(rooms)
                            if rooms.iter().any(|r| r.id == room_id && !r.is_pending()) =>
                        {
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("bob join timed out");

            h_b.send(MatrixCmd::OpenRoom {
                room_id: room_id.clone(),
                generation: 1,
            });
            let power = wait_power(&mut ev_b, &room_id, |_| true).await;
            assert!(!power.can_ban && !power.can_pin && !power.can_redact_other);

            // Bob sends a message and gets its event_id.
            h_b.send(MatrixCmd::Send {
                room_id: room_id.clone(),
                message: format::parse("bob plain msg"),
                reply_to: None,
            });
            let sent = wait_timeline_msg(&mut ev_b, &room_id, |msgs| {
                msgs.iter()
                    .find(|m| m.body.contains("bob plain msg") && m.event_id().is_some())
                    .cloned()
            })
            .await;
            let event_id = sent.event_id().unwrap().to_owned();

            // Trying to pin fails with ActionFailed.
            h_b.send(MatrixCmd::Pin {
                room_id: room_id.clone(),
                event_id,
                pinned: true,
            });
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match ev_b.recv().await.expect("worker stopped") {
                        MatrixEvent::ActionFailed(msg) => {
                            assert!(!msg.is_empty());
                            break;
                        }
                        MatrixEvent::PinnedEvents { .. } => {
                            panic!("unprivileged user should not be able to pin")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("timed out waiting for Pin rejection");

            // Trying to ban Alice fails cleanly.
            h_b.send(MatrixCmd::Manage(ManageCmd::Ban {
                room_id: room_id.clone(),
                user_id: alice_id,
                reason: None,
            }));
            let ban_res = wait_done(&mut ev_b, Change::Ban).await;
            assert!(ban_res.is_err(), "expected ban rejection: {ban_res:?}");
        })
        .catch_unwind()
        .await;

        cleanup_rooms(
            vec![
                (&h_a, &mut ev_a, &cleanup_ids),
                (&h_b, &mut ev_b, &cleanup_ids),
            ],
            None,
            &[],
            vec![root_a, root_b],
        )
        .await;
        if let Err(err) = outcome {
            std::panic::resume_unwind(err);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn server_admin_set_role_ban_and_unban() {
        let hs = hs();
        let alice_user = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let alice_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let (root_a, h_a, mut ev_a, _) =
            login_worker("roleban-a", &hs, &alice_user, &alice_pass).await;
        let (bob, bob_id) = login_bob(&hs).await;
        let room_id = create_room(&h_a, &mut ev_a, "matrix-rat mod roleban test", false).await;
        let cleanup_ids = vec![room_id.clone()];

        let outcome = AssertUnwindSafe(async {
            wait_info(&h_a, &mut ev_a, &room_id, "creator-admin", |d| {
                d.members
                    .iter()
                    .any(|m| m.own && m.role >= MemberRole::Admin)
            })
            .await;
            invite_and_join_bob(&h_a, &mut ev_a, &bob, &room_id, &bob_id).await;
            let details = wait_info(&h_a, &mut ev_a, &room_id, "bob-joined", |d| {
                d.members
                    .iter()
                    .any(|m| m.own && m.role >= MemberRole::Admin)
                    && d.members
                        .iter()
                        .any(|m| m.user_id == bob_id && !m.invited && !m.banned)
            })
            .await;
            let diag: Vec<_> = details
                .members
                .iter()
                .map(|m| {
                    (
                        m.own,
                        m.role,
                        m.power_level,
                        m.bannable,
                        m.removable,
                        m.role_editable,
                    )
                })
                .collect();
            assert!(
                details
                    .members
                    .iter()
                    .any(|m| m.user_id == bob_id && m.bannable),
                "own_pl={}, creator={}, members={diag:?}",
                details.own_power_level,
                details.own_is_creator
            );

            // Promote Bob to Moderator (50), then reset to Default (0).
            h_a.send(MatrixCmd::Manage(ManageCmd::SetRole {
                room_id: room_id.clone(),
                user_id: bob_id.clone(),
                level: 50,
            }));
            wait_done(&mut ev_a, Change::SetRole).await.unwrap();
            let details = wait_info(&h_a, &mut ev_a, &room_id, "bob-mod", |d| {
                d.members
                    .iter()
                    .any(|m| m.user_id == bob_id && m.power_level == 50)
            })
            .await;
            let bob_row = details
                .members
                .iter()
                .find(|m| m.user_id == bob_id)
                .unwrap();
            assert_eq!(bob_row.role, MemberRole::Moderator);

            // Ban Bob with a reason.
            h_a.send(MatrixCmd::Manage(ManageCmd::Ban {
                room_id: room_id.clone(),
                user_id: bob_id.clone(),
                reason: Some("server integration test".into()),
            }));
            wait_done(&mut ev_a, Change::Ban).await.unwrap();
            let details = wait_info(&h_a, &mut ev_a, &room_id, "bob-banned", |d| {
                d.members.iter().any(|m| m.user_id == bob_id && m.banned)
            })
            .await;
            let banned_bob = details
                .members
                .iter()
                .find(|m| m.user_id == bob_id)
                .unwrap();
            assert!(banned_bob.banned && banned_bob.unbannable);

            // Unban Bob -> no longer listed among banned or joined members.
            h_a.send(MatrixCmd::Manage(ManageCmd::Unban {
                room_id: room_id.clone(),
                user_id: bob_id.clone(),
                reason: None,
            }));
            wait_done(&mut ev_a, Change::Unban).await.unwrap();
            wait_info(&h_a, &mut ev_a, &room_id, "bob-unbanned", |d| {
                !d.members.iter().any(|m| m.user_id == bob_id)
            })
            .await;
        })
        .catch_unwind()
        .await;

        cleanup_rooms(
            vec![(&h_a, &mut ev_a, &cleanup_ids)],
            Some(&bob),
            &cleanup_ids,
            vec![root_a],
        )
        .await;
        if let Err(err) = outcome {
            std::panic::resume_unwind(err);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn server_pin_and_unpin_sync_and_fetch_unloaded() {
        let hs = hs();
        let alice_user = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let alice_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let (root1, h1, mut ev1, _) = login_worker("pin1", &hs, &alice_user, &alice_pass).await;
        let (root2, h2, mut ev2, _) = login_worker("pin2", &hs, &alice_user, &alice_pass).await;

        let room_id = create_room(&h1, &mut ev1, "matrix-rat mod pin test", false).await;
        let cleanup_ids = vec![room_id.clone()];

        let outcome = AssertUnwindSafe(async {
            // Wait for second client to know about the room before fetching pinned.
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match ev2.recv().await.expect("worker stopped") {
                        MatrixEvent::Rooms(rooms) if rooms.iter().any(|r| r.id == room_id) => break,
                        _ => {}
                    }
                }
            })
            .await
            .expect("second client did not see room");

            h1.send(MatrixCmd::OpenRoom {
                room_id: room_id.clone(),
                generation: 1,
            });
            h1.send(MatrixCmd::Send {
                room_id: room_id.clone(),
                message: format::parse("pin worthy message"),
                reply_to: None,
            });
            let msg = wait_timeline_msg(&mut ev1, &room_id, |msgs| {
                msgs.iter()
                    .find(|m| m.body.contains("pin worthy message") && m.event_id().is_some())
                    .cloned()
            })
            .await;
            let event_id = msg.event_id().unwrap().to_owned();

            // Pin from client 1.
            h1.send(MatrixCmd::Pin {
                room_id: room_id.clone(),
                event_id: event_id.clone(),
                pinned: true,
            });
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match ev1.recv().await.expect("worker stopped") {
                        MatrixEvent::PinnedEvents { room_id: id, ids }
                            if id == room_id && ids.contains(&event_id) =>
                        {
                            break;
                        }
                        MatrixEvent::ActionFailed(err) | MatrixEvent::Error(err) => {
                            panic!("pin failed: {err}")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("timed out waiting for PinnedEvents");

            // Client 2 (without having opened the room's timeline!) fetches pinned events via load_or_fetch_event.
            h2.send(MatrixCmd::FetchPinned {
                room_id: room_id.clone(),
            });
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    match ev2.recv().await.expect("worker stopped") {
                        MatrixEvent::PinnedList {
                            room_id: id,
                            items,
                            error,
                        } if id == room_id => {
                            if items.iter().any(|it| {
                                it.event_id == event_id && it.snippet.contains("pin worthy")
                            }) {
                                assert!(error.is_none());
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(250)).await;
                            h2.send(MatrixCmd::FetchPinned {
                                room_id: room_id.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("client 2 did not fetch pinned message");

            // Unpin from client 1 -> FetchPinned on client 2 returns empty.
            h1.send(MatrixCmd::Pin {
                room_id: room_id.clone(),
                event_id: event_id.clone(),
                pinned: false,
            });
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let MatrixEvent::PinnedEvents { room_id: id, ids } =
                        ev1.recv().await.expect("worker stopped")
                        && id == room_id
                        && ids.is_empty()
                    {
                        break;
                    }
                }
            })
            .await
            .expect("timed out waiting for unpin");

            h2.send(MatrixCmd::FetchPinned {
                room_id: room_id.clone(),
            });
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    if let MatrixEvent::PinnedList {
                        room_id: id,
                        items,
                        error: None,
                    } = ev2.recv().await.expect("worker stopped")
                        && id == room_id
                    {
                        if items.is_empty() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        h2.send(MatrixCmd::FetchPinned {
                            room_id: room_id.clone(),
                        });
                    }
                }
            })
            .await
            .expect("client 2 did not see unpinned empty list");
        })
        .catch_unwind()
        .await;

        cleanup_rooms(
            vec![(&h1, &mut ev1, &cleanup_ids), (&h2, &mut ev2, &[])],
            None,
            &[],
            vec![root1, root2],
        )
        .await;
        if let Err(err) = outcome {
            std::panic::resume_unwind(err);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn server_forward_text_reply_and_attachment_as_standalone_copies() {
        let hs = hs();
        let alice_user = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let alice_pass =
            std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let (root, handle, mut events, _) =
            login_worker("forward", &hs, &alice_user, &alice_pass).await;

        let src_room = create_room(&handle, &mut events, "matrix-rat mod fwd src", false).await;
        let dst_room = create_room(&handle, &mut events, "matrix-rat mod fwd dst", false).await;
        let cleanup_ids = vec![src_room.clone(), dst_room.clone()];
        let sample_file = root.join("forward-note.txt");
        std::fs::write(&sample_file, b"hello forwarded attachment\n").unwrap();

        let outcome = AssertUnwindSafe(async {
            handle.send(MatrixCmd::OpenRoom {
                room_id: src_room.clone(),
                generation: 1,
            });
            handle.send(MatrixCmd::Send {
                room_id: src_room.clone(),
                message: format::parse("root message"),
                reply_to: None,
            });
            let base = wait_timeline_msg(&mut events, &src_room, |msgs| {
                msgs.iter()
                    .find(|m| m.body.contains("root message") && m.event_id().is_some())
                    .cloned()
            })
            .await;
            let base_id = base.event_id().unwrap().to_owned();

            // Send a reply to strip when forwarded, plus a file attachment.
            handle.send(MatrixCmd::Send {
                room_id: src_room.clone(),
                message: format::parse("forwarded standalone reply"),
                reply_to: Some(base_id),
            });
            let reply_msg = wait_timeline_msg(&mut events, &src_room, |msgs| {
                msgs.iter()
                    .find(|m| {
                        m.body.contains("forwarded standalone reply") && m.event_id().is_some()
                    })
                    .cloned()
            })
            .await;
            let reply_event_id = reply_msg.event_id().unwrap().to_owned();

            handle.send(MatrixCmd::SendFiles {
                send: 0,
                room_id: src_room.clone(),
                files: vec![sample_file.clone().into()],
                caption: None,
                reply_to: None,
            });
            let file_msg = wait_timeline_msg(&mut events, &src_room, |msgs| {
                msgs.iter()
                    .find(|m| m.file.is_some() && m.event_id().is_some())
                    .cloned()
            })
            .await;
            let file_event_id = file_msg.event_id().unwrap().to_owned();

            // Forward both to dst_room.
            for eid in [reply_event_id, file_event_id] {
                handle.send(MatrixCmd::Forward {
                    from_room_id: src_room.clone(),
                    event_id: eid,
                    to_room_id: dst_room.clone(),
                });
                tokio::time::timeout(Duration::from_secs(60), async {
                    loop {
                        match events.recv().await.expect("worker stopped") {
                            MatrixEvent::ForwardResult {
                                to_room_id, result, ..
                            } if to_room_id == dst_room => {
                                result.expect("ForwardResult failed");
                                break;
                            }
                            MatrixEvent::Error(err) => panic!("{err}"),
                            _ => {}
                        }
                    }
                })
                .await
                .expect("timed out waiting for ForwardResult");
            }

            // Open dst_room and verify standalone reply + file arrived without reply relation.
            // Wait for both in one pass because both may already be present in the initial snapshot.
            handle.send(MatrixCmd::OpenRoom {
                room_id: dst_room.clone(),
                generation: 2,
            });
            let copied_text = wait_timeline_msg(&mut events, &dst_room, |msgs| {
                let text = msgs
                    .iter()
                    .find(|m| m.body.contains("forwarded standalone reply"))?;
                let has_file = msgs.iter().any(|m| {
                    m.file
                        .as_ref()
                        .is_some_and(|f| f.name == "forward-note.txt")
                });
                has_file.then(|| text.clone())
            })
            .await;
            assert!(
                copied_text.reply_to.is_none() && copied_text.reply_event.is_none(),
                "expected standalone forward without reply relation: {copied_text:?}"
            );
        })
        .catch_unwind()
        .await;

        cleanup_rooms(
            vec![(&handle, &mut events, &cleanup_ids)],
            None,
            &[],
            vec![root],
        )
        .await;
        if let Err(err) = outcome {
            std::panic::resume_unwind(err);
        }
    }
}
