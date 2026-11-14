//! Global full-text search over locally indexed messages across joined rooms.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use chrono::{DateTime, Local};
use futures::StreamExt;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk::event_cache::{RedecryptorReport, RoomEventCache};
use matrix_sdk::ruma::events::relation::RelationType;
use matrix_sdk::ruma::events::room::encrypted::OriginalSyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, Relation, RoomMessageEventContent, SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, OriginalSyncMessageLikeEvent,
};
use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId};
use matrix_sdk::{Client, Room, RoomState};
use matrix_sdk_base::event_cache::store::EventCacheStoreLockState;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use unicode_segmentation::UnicodeSegmentation;

use super::safety::{IgnoredSet, is_ignored_in};
use super::{MatrixEvent, convert};

const SEARCH_BATCH_SIZE: usize = 16;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SNIPPET_CHARS: usize = 400;

#[derive(Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub room_id: String,
    pub event_id: String,
    pub thread_root: Option<String>,
    pub room_name: String,
    pub sender: String,
    pub date: String,
    pub snippet: String,
}

/// Never logs room names, sender display names, or message snippets.
impl fmt::Debug for SearchHit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SearchHit({}, {}, thread={})",
            self.room_id,
            self.event_id,
            self.thread_root.as_deref().unwrap_or("-")
        )
    }
}

/// Removes persisted per-room search index directories whose room IDs are no longer
/// joined. Called when no `RoomIndex` handles are open (startup before sync/search, and
/// session shutdown after dropping the `Client`) because `matrix-sdk 0.19.1` keeps an
/// in-memory `HashMap<OwnedRoomId, RoomIndex>` without a public room-removal API.
pub(crate) fn prune_search_dir(search_dir: &Path, joined_room_ids: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(search_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if RoomId::parse(&name).is_ok()
            && !joined_room_ids.contains(&name)
            && let Err(err) = std::fs::remove_dir_all(entry.path())
        {
            tracing::debug!(%err, room = %name, "could not prune stale search index");
        }
    }
}

/// Returns the IDs of every currently joined room.
pub(crate) fn joined_room_ids(client: &Client) -> HashSet<String> {
    client
        .joined_rooms()
        .into_iter()
        .map(|room| room.room_id().to_string())
        .collect()
}

/// Prunes non-joined room search index directories using the client's restored room list.
pub(crate) fn prune_stale_search_indexes(client: &Client, search_dir: &Path) {
    let joined = joined_room_ids(client);
    prune_search_dir(search_dir, &joined);
}

pub(super) struct Searcher {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    ignored: watch::Receiver<IgnoredSet>,
    task: Option<JoinHandle<()>>,
    redecrypt_task: Option<JoinHandle<()>>,
}

impl Searcher {
    pub(super) fn new(
        client: Client,
        events: UnboundedSender<MatrixEvent>,
        ignored: watch::Receiver<IgnoredSet>,
    ) -> Self {
        let redecrypt_task = Some(tokio::spawn(index_redecrypted(client.clone())));
        Self {
            client,
            events,
            ignored,
            task: None,
            redecrypt_task,
        }
    }

    pub(super) fn start(&mut self, generation: u64, query: String) {
        self.cancel();
        let client = self.client.clone();
        let events = self.events.clone();
        let ignored = self.ignored.clone();
        self.task = Some(tokio::spawn(async move {
            run_search(client, events, ignored, generation, query).await;
        }));
    }

    pub(super) fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    pub(super) fn stop(&mut self) {
        self.cancel();
        if let Some(task) = self.redecrypt_task.take() {
            task.abort();
        }
    }
}

impl Drop for Searcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Indexes events that the SDK's `Redecryptor` decrypted asynchronously.
///
/// In matrix-sdk 0.19.1, `Redecryptor::on_resolved_utds` writes decrypted UTDs
/// back into the event cache and timeline subscribers via
/// `RoomEventCacheUpdate`, bypassing `RoomEventCacheLinkedChunkUpdate` where
/// the SDK's `search_indexing_task` listens. Forwarding `ResolvedUtds` reports
/// keeps encrypted rooms searchable once keys arrive later.
async fn index_redecrypted(client: Client) {
    let stream = client.event_cache().subscribe_to_decryption_reports();
    tokio::pin!(stream);
    while let Some(item) = stream.next().await {
        let Ok(RedecryptorReport::ResolvedUtds { room_id, events }) = item else {
            continue;
        };
        if events.is_empty() {
            continue;
        }
        let Some(room) = client.get_room(&room_id) else {
            continue;
        };
        let Ok((room_cache, _drops)) = client.event_cache().room(&room_id).await else {
            continue;
        };
        let mut resolved = Vec::with_capacity(events.len());
        for event_id in events {
            let Ok(Some(mut event)) = room_cache.find_event(&event_id).await else {
                continue;
            };
            if event.kind.is_utd()
                && let Some(decrypted) = try_decrypt_utd(&room, &event).await
            {
                event = decrypted;
            }
            if !event.kind.is_utd() {
                resolved.push(event);
            }
        }
        if resolved.is_empty() {
            continue;
        }
        let redaction_rules = room.clone_info().room_version_rules_or_default().redaction;
        let mut pending = resolved;
        for _ in 0..3 {
            for event in &pending {
                ensure_decrypted_in_store(&client, &room_id, &room_cache, event).await;
            }
            {
                let mut guard = client.search_index().lock().await;
                if let Err(err) = guard
                    .bulk_handle_timeline_event(
                        pending.iter().cloned(),
                        &room_cache,
                        &room_id,
                        &redaction_rules,
                    )
                    .await
                {
                    tracing::warn!(%err, %room_id, "failed to index redecrypted events");
                    break;
                }
            }
            // In matrix-sdk 0.19.1, `post_process_upserted_events` saves
            // `bundled_latest_thread_event` with `INSERT OR REPLACE INTO events`,
            // which can overwrite a decrypted thread reply in SQLite with an
            // encrypted UTD copy and make `get_most_recent_edit` skip indexing it.
            let mut overwritten = Vec::new();
            for event in pending {
                if store_missing_or_utd(&room_cache, &event).await {
                    overwritten.push(event);
                }
            }
            if overwritten.is_empty() {
                break;
            }
            pending = overwritten;
        }
    }
}

async fn store_missing_or_utd(room_cache: &RoomEventCache, event: &TimelineEvent) -> bool {
    let Some(event_id) = event.event_id() else {
        return false;
    };
    !matches!(
        room_cache
            .find_event_with_relations(event_id, Some(vec![RelationType::Replacement]))
            .await,
        Ok(Some((stored, _))) if !stored.kind.is_utd()
    )
}

async fn ensure_decrypted_in_store(
    client: &Client,
    room_id: &RoomId,
    room_cache: &RoomEventCache,
    event: &TimelineEvent,
) {
    if event.kind.is_utd() || !store_missing_or_utd(room_cache, event).await {
        return;
    }
    if let Ok(lock_state) = client.event_cache_store().lock().await {
        let (EventCacheStoreLockState::Clean(store) | EventCacheStoreLockState::Dirty(store)) =
            lock_state;
        let _ = store.save_event(room_id, event.clone()).await;
    }
}

async fn try_decrypt_utd(room: &Room, event: &TimelineEvent) -> Option<TimelineEvent> {
    if !event.kind.is_utd() {
        return None;
    }
    let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(enc))) =
        event.raw().deserialize()
    else {
        return None;
    };
    enc.as_original()?;
    let decrypted = room
        .decrypt_event(
            event
                .raw()
                .cast_ref_unchecked::<OriginalSyncRoomEncryptedEvent>(),
            None,
        )
        .await
        .ok()?;
    (!decrypted.kind.is_utd()).then_some(decrypted)
}

async fn run_search(
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    ignored: watch::Receiver<IgnoredSet>,
    generation: u64,
    query: String,
) {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        let _ = events.send(MatrixEvent::SearchResults {
            generation,
            hits: Vec::new(),
            done: true,
            note: None,
        });
        return;
    }

    let now = Local::now();
    let mut room_names = HashMap::new();
    let mut sender_names = HashMap::new();
    let mut seen: HashSet<(OwnedRoomId, String)> = HashSet::new();
    let mut batch = Vec::with_capacity(SEARCH_BATCH_SIZE);
    let mut emitted_any = false;
    let mut resolved_count = 0usize;
    let mut active_query = trimmed.to_owned();
    let builder = client.search_messages(active_query.clone());
    let mut stream = Box::pin(builder.build());
    let mut note = None;

    'outer: while let Some(page) = stream.next().await {
        let entries = match page {
            Ok(entries) => entries,
            Err(err) => {
                // Retry once with operator punctuation stripped if an incomplete syntax
                // token (unmatched quote/paren, URL scheme colon) failed parsing.
                if !emitted_any && batch.is_empty() && active_query == trimmed {
                    let fallback = sanitize_query(trimmed);
                    if !fallback.is_empty() && fallback != trimmed {
                        active_query = fallback;
                        stream = Box::pin(client.search_messages(active_query.clone()).build());
                        continue;
                    }
                    if fallback.is_empty() {
                        break;
                    }
                }
                tracing::debug!(%err, generation, "global message search failed");
                note = Some("Could not search that query".to_owned());
                break;
            }
        };

        for (room_id, _score, event_id) in entries {
            let Some(hit) = resolve_hit(
                &client,
                &ignored,
                &room_id,
                &event_id,
                &active_query,
                &now,
                &mut room_names,
                &mut sender_names,
            )
            .await
            else {
                continue;
            };
            if !seen.insert((room_id, hit.event_id.clone())) {
                continue;
            }

            if resolved_count >= MAX_SEARCH_RESULTS {
                note = Some(format!(
                    "Showing first {MAX_SEARCH_RESULTS} matches — refine your search"
                ));
                break 'outer;
            }

            resolved_count += 1;
            batch.push(hit);
            if batch.len() >= SEARCH_BATCH_SIZE {
                emitted_any = true;
                let _ = events.send(MatrixEvent::SearchResults {
                    generation,
                    hits: std::mem::take(&mut batch),
                    done: false,
                    note: None,
                });
            }
        }
    }

    let _ = events.send(MatrixEvent::SearchResults {
        generation,
        hits: batch,
        done: true,
        note,
    });
}

#[allow(clippy::too_many_arguments)]
async fn resolve_hit(
    client: &Client,
    ignored: &watch::Receiver<IgnoredSet>,
    room_id: &OwnedRoomId,
    event_id: &EventId,
    query: &str,
    now: &DateTime<Local>,
    room_names: &mut HashMap<OwnedRoomId, String>,
    sender_names: &mut HashMap<(OwnedRoomId, OwnedUserId), String>,
) -> Option<SearchHit> {
    let room = client.get_room(room_id)?;
    if room.state() != RoomState::Joined {
        return None;
    }

    let (mut event, mut relations) = load_event_with_replacements(&room, event_id).await?;
    if event
        .sender()
        .is_some_and(|sender| is_ignored_in(ignored, &sender))
    {
        return None;
    }
    let mut resolved_event_id = event_id.to_owned();
    // When a message has been edited, the search index returns the edit event's ID as
    // primary key. Unwrap to the original target event so SearchHit carries its
    // canonical ItemRef event ID, thread root relation, timestamp, and validated text.
    if let Some(target_id) = replacement_target(&event)
        && target_id != resolved_event_id
        && let Some((orig_event, orig_relations)) =
            load_event_with_replacements(&room, &target_id).await
    {
        event = orig_event;
        relations = orig_relations;
        resolved_event_id = target_id;
    }

    let sender_id = event.sender()?;
    if is_ignored_in(ignored, &sender_id) {
        return None;
    }

    let body = extract_event_text(&event, &relations)?;
    let snippet = window_snippet(&body, query);
    if snippet.is_empty() {
        return None;
    }
    let thread_root = extract_thread_root(&event);

    let room_name = cached_room_name(&room, room_names).await;
    let sender = cached_sender_name(&room, &sender_id, sender_names).await;
    let date = event
        .timestamp()
        .and_then(convert::local_time)
        .map(|when| convert::day_label(&when, now))
        .unwrap_or_default();

    Some(SearchHit {
        room_id: room_id.to_string(),
        event_id: resolved_event_id.to_string(),
        thread_root,
        room_name,
        sender,
        date,
        snippet,
    })
}

fn replacement_target(event: &TimelineEvent) -> Option<OwnedEventId> {
    let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg))) =
        event.raw().deserialize()
    else {
        return None;
    };
    let orig = msg.as_original()?;
    match &orig.content.relates_to {
        Some(Relation::Replacement(re)) => Some(re.event_id.clone()),
        _ => None,
    }
}

fn extract_thread_root(event: &TimelineEvent) -> Option<String> {
    let Ok(AnySyncTimelineEvent::MessageLike(msg)) = event.raw().deserialize() else {
        return None;
    };
    if let AnySyncMessageLikeEvent::RoomMessage(room_msg) = msg {
        let orig = room_msg.as_original()?;
        return match &orig.content.relates_to {
            Some(Relation::Thread(thread)) => Some(thread.event_id.to_string()),
            _ => None,
        };
    }
    thread_root_from_raw(event.raw().json().get())
}

fn thread_root_from_raw(raw_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw_json).ok()?;
    let rel = value.get("content")?.get("m.relates_to")?;
    if rel.get("rel_type")?.as_str()? != "m.thread" {
        return None;
    }
    let id = rel.get("event_id")?.as_str()?;
    EventId::parse(id).ok().map(|ev| ev.to_string())
}

async fn heal_utd_event(
    room: &Room,
    cache: &RoomEventCache,
    mut event: TimelineEvent,
) -> TimelineEvent {
    if !event.kind.is_utd() {
        return event;
    }
    if let Some(id) = event.event_id()
        && let Ok(Some(cached)) = cache.find_event(id).await
        && !cached.kind.is_utd()
    {
        event = cached;
    } else if let Some(decrypted) = try_decrypt_utd(room, &event).await {
        event = decrypted;
    }
    if !event.kind.is_utd() {
        let client = room.client();
        ensure_decrypted_in_store(&client, room.room_id(), cache, &event).await;
    }
    event
}

async fn load_event_with_replacements(
    room: &Room,
    event_id: &EventId,
) -> Option<(TimelineEvent, Vec<TimelineEvent>)> {
    if let Ok((cache, _drops)) = room.event_cache().await
        && let Ok(Some((event, mut edits))) = cache
            .find_event_with_relations(event_id, Some(vec![RelationType::Replacement]))
            .await
    {
        let event = heal_utd_event(room, &cache, event).await;
        for edit in &mut edits {
            if edit.kind.is_utd() {
                let replacement = heal_utd_event(room, &cache, edit.clone()).await;
                *edit = replacement;
            }
        }
        return Some((event, edits));
    }

    room.load_or_fetch_event(event_id, None)
        .await
        .ok()
        .map(|event| (event, Vec::new()))
}

fn extract_event_text(event: &TimelineEvent, relations: &[TimelineEvent]) -> Option<String> {
    let Ok(AnySyncTimelineEvent::MessageLike(msg)) = event.raw().deserialize() else {
        return None;
    };
    match msg {
        AnySyncMessageLikeEvent::RoomMessage(msg) => {
            let orig = msg.as_original()?;
            // Prefer the latest replacement from the original sender in the event cache
            // or in the event's bundled relations so edits show the current text.
            let replaced = latest_replacement(relations, &orig.event_id, &orig.sender)
                .or_else(|| bundled_replacement(&orig.unsigned.relations.replace, &orig.sender));
            let msgtype = replaced.as_ref().unwrap_or(&orig.content.msgtype);
            Some(msgtype.body().to_owned())
        }
        AnySyncMessageLikeEvent::Sticker(sticker) => {
            Some(sticker.as_original()?.content.body.clone())
        }
        AnySyncMessageLikeEvent::PollStart(poll) => poll
            .as_original()?
            .content
            .poll
            .question
            .text
            .find_plain()
            .map(str::to_owned),
        AnySyncMessageLikeEvent::UnstablePollStart(poll) => Some(
            poll.as_original()?
                .content
                .poll_start()
                .question
                .text
                .clone(),
        ),
        _ => None,
    }
}

fn latest_replacement(
    relations: &[TimelineEvent],
    target_id: &EventId,
    orig_sender: &UserId,
) -> Option<MessageType> {
    let mut best: Option<(u64, MessageType)> = None;
    for rel in relations {
        let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(ev),
        ))) = rel.raw().deserialize()
        else {
            continue;
        };
        if ev.sender != orig_sender {
            continue;
        }
        if let Some(Relation::Replacement(re)) = ev.content.relates_to
            && re.event_id == target_id
        {
            let ts: u64 = ev.origin_server_ts.get().into();
            if best.as_ref().is_none_or(|(best_ts, _)| ts >= *best_ts) {
                best = Some((ts, re.new_content.msgtype));
            }
        }
    }
    best.map(|(_, msgtype)| msgtype)
}

fn bundled_replacement(
    replace: &Option<Box<OriginalSyncMessageLikeEvent<RoomMessageEventContent>>>,
    orig_sender: &UserId,
) -> Option<MessageType> {
    let ev = replace.as_deref()?;
    if ev.sender != orig_sender {
        return None;
    }
    match &ev.content.relates_to {
        Some(Relation::Replacement(re)) => Some(re.new_content.msgtype.clone()),
        _ => None,
    }
}

async fn cached_room_name(room: &Room, cache: &mut HashMap<OwnedRoomId, String>) -> String {
    let id = room.room_id().to_owned();
    if let Some(name) = cache.get(&id) {
        return name.clone();
    }
    let raw = match room.cached_display_name() {
        Some(name) => name.to_string(),
        None => room
            .display_name()
            .await
            .map(|name| name.to_string())
            .unwrap_or_else(|_| id.to_string()),
    };
    let clean = crate::text::sanitize_line(&raw);
    cache.insert(id, clean.clone());
    clean
}

async fn cached_sender_name(
    room: &Room,
    user_id: &UserId,
    cache: &mut HashMap<(OwnedRoomId, OwnedUserId), String>,
) -> String {
    let key = (room.room_id().to_owned(), user_id.to_owned());
    if let Some(name) = cache.get(&key) {
        return name.clone();
    }
    let member_name = room
        .get_member_no_sync(user_id)
        .await
        .ok()
        .flatten()
        .map(|member| crate::text::sanitize_line(member.name()))
        .filter(|name| !crate::text::is_blank(name))
        .unwrap_or_else(|| crate::text::sanitize_line(user_id.localpart()));
    cache.insert(key, member_name.clone());
    member_name
}

/// Strips Tantivy query-parser syntax punctuation so partial quotes, parentheses, or URL
/// colons still work as plain-term queries when the strict parser rejects them.
pub(crate) fn sanitize_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let mut prev_space = true;
    for ch in query.chars() {
        let syntax = matches!(
            ch,
            '+' | '-'
                | '&'
                | '|'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '^'
                | '"'
                | '~'
                | '*'
                | '?'
                | ':'
                | '\\'
                | '/'
        );
        if syntax || ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Flattens `text` to one safe line, keeping a bounded window centered on the first match
/// when a long message exceeds `MAX_SNIPPET_CHARS`.
pub(crate) fn window_snippet(text: &str, query: &str) -> String {
    let line = crate::text::sanitize_line(text);
    let trimmed = line.trim();
    let total = trimmed.graphemes(true).count();
    if total <= MAX_SNIPPET_CHARS {
        return trimmed.to_owned();
    }

    let match_byte = first_term_match_span(trimmed, query).map_or(0, |(start, _)| start);
    let mut start_grapheme = trimmed[..match_byte].graphemes(true).count();
    let half = MAX_SNIPPET_CHARS / 2;
    start_grapheme = start_grapheme
        .saturating_sub(half)
        .min(total.saturating_sub(MAX_SNIPPET_CHARS));
    let end_grapheme = (start_grapheme + MAX_SNIPPET_CHARS).min(total);

    let mut out = String::new();
    if start_grapheme > 0 {
        out.push('…');
    }
    for g in trimmed
        .graphemes(true)
        .skip(start_grapheme)
        .take(end_grapheme - start_grapheme)
    {
        out.push_str(g);
    }
    if end_grapheme < total {
        out.push('…');
    }
    out
}

pub(crate) fn first_term_match_span(text: &str, query: &str) -> Option<(usize, usize)> {
    let lower_text = text.to_lowercase();
    query_terms(query)
        .into_iter()
        .filter_map(|term| {
            let lower_term = term.to_lowercase();
            let byte = lower_text.find(&lower_term)?;
            let start = floor_char_in(text, &lower_text, byte)?;
            let end = floor_char_in(text, &lower_text, byte + lower_term.len())?;
            (start < end && end <= text.len()).then_some((start, end))
        })
        .min_by_key(|&(start, _)| start)
}

fn floor_char_in(orig: &str, lower: &str, lower_byte: usize) -> Option<usize> {
    // Only exact ASCII/identical scalar byte alignment guarantees 1:1 offset; fall back to
    // counting lowercase chars to stay panic-free on casefold length changes (e.g. 'ß' / 'İ').
    if orig.len() == lower.len() && orig.is_char_boundary(lower_byte) {
        return Some(lower_byte);
    }
    let prefix = lower.get(..lower_byte)?;
    let char_idx = prefix.chars().count();
    Some(
        orig.char_indices()
            .nth(char_idx)
            .map_or(orig.len(), |(byte, _)| byte),
    )
}

pub(crate) fn query_terms(query: &str) -> Vec<&str> {
    query
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '+' | '-' | '!' | '*' | '?'
                )
            })
        })
        .filter(|w| !w.is_empty() && !matches!(*w, "AND" | "OR" | "NOT"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use matrix_sdk::ruma::api::client::room::create_room::v3::Request as CreateRoom;
    use matrix_sdk::ruma::api::client::uiaa::{AuthData, RegistrationToken};
    use matrix_sdk::ruma::events::InitialStateEvent;
    use matrix_sdk::ruma::events::room::encryption::RoomEncryptionEventContent;
    use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::matrix::{MatrixCmd, MatrixHandle, spawn};
    use crate::model::MsgKind;
    use crate::paths::Paths;

    #[test]
    fn sanitize_query_handles_syntax_characters() {
        assert_eq!(
            sanitize_query("http://example.com:8080/chat"),
            "http example.com 8080 chat"
        );
        assert_eq!(sanitize_query("\"unclosed (paren"), "unclosed paren");
        assert_eq!(sanitize_query(":::"), "");
    }

    #[test]
    fn window_snippet_centers_far_match() {
        let pad = "word ".repeat(120);
        let text = format!("{pad}needle_here trailing");
        let snippet = window_snippet(&text, "needle_here");
        assert!(snippet.contains("needle_here"), "{snippet}");
        assert!(snippet.starts_with('…'), "{snippet}");
        assert!(snippet.graphemes(true).count() <= MAX_SNIPPET_CHARS + 2);
    }

    #[test]
    fn search_hit_debug_redacts_content() {
        let hit = SearchHit {
            room_id: "!room:example.org".into(),
            event_id: "$ev1:example.org".into(),
            thread_root: Some("$root:example.org".into()),
            room_name: "Top Secret Room".into(),
            sender: "Secret Sender".into(),
            date: "Today".into(),
            snippet: "private message body".into(),
        };
        let dbg = format!("{hit:?}");
        assert!(!dbg.contains("Top Secret Room"));
        assert!(!dbg.contains("Secret Sender"));
        assert!(!dbg.contains("private message body"));
        assert!(dbg.contains("!room:example.org"));
        assert!(dbg.contains("$ev1:example.org"));
        assert!(dbg.contains("$root:example.org"));

        let cmd = MatrixCmd::SearchMessages {
            generation: 7,
            query: "secret query text".into(),
        };
        let cmd_dbg = format!("{cmd:?}");
        assert!(!cmd_dbg.contains("secret query text"), "{cmd_dbg}");

        let ev = MatrixEvent::SearchResults {
            generation: 7,
            hits: vec![hit],
            done: true,
            note: Some("note detail".into()),
        };
        let ev_dbg = format!("{ev:?}");
        assert!(!ev_dbg.contains("private message body"), "{ev_dbg}");
        assert!(!ev_dbg.contains("note detail"), "{ev_dbg}");
    }

    #[test]
    fn extract_thread_root_and_replacement_target_parse_relations() {
        let make = |raw: serde_json::Value| {
            let raw_ev: matrix_sdk::ruma::serde::Raw<AnySyncTimelineEvent> =
                serde_json::from_value(raw).expect("valid raw timeline event");
            TimelineEvent::from_plaintext(raw_ev)
        };

        let plain = make(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$plain:example.org",
            "sender": "@alice:example.org",
            "origin_server_ts": 1000,
            "content": { "msgtype": "m.text", "body": "hi" }
        }));
        assert_eq!(extract_thread_root(&plain), None);
        assert_eq!(replacement_target(&plain), None);

        let reply_only = make(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$reply:example.org",
            "sender": "@alice:example.org",
            "origin_server_ts": 1001,
            "content": {
                "msgtype": "m.text",
                "body": "reply",
                "m.relates_to": { "m.in_reply_to": { "event_id": "$root:example.org" } }
            }
        }));
        assert_eq!(extract_thread_root(&reply_only), None);

        let threaded = make(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$thr:example.org",
            "sender": "@alice:example.org",
            "origin_server_ts": 1002,
            "content": {
                "msgtype": "m.text",
                "body": "in thread",
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": "$root:example.org",
                    "m.in_reply_to": { "event_id": "$root:example.org" },
                    "is_falling_back": true
                }
            }
        }));
        assert_eq!(
            extract_thread_root(&threaded).as_deref(),
            Some("$root:example.org")
        );

        let edited = make(serde_json::json!({
            "type": "m.room.message",
            "event_id": "$edit:example.org",
            "sender": "@alice:example.org",
            "origin_server_ts": 1003,
            "content": {
                "msgtype": "m.text",
                "body": "* edited",
                "m.new_content": { "msgtype": "m.text", "body": "edited" },
                "m.relates_to": {
                    "rel_type": "m.replace",
                    "event_id": "$thr:example.org"
                }
            }
        }));
        assert_eq!(
            replacement_target(&edited).as_ref().map(|id| id.as_str()),
            Some("$thr:example.org")
        );

        let threaded_sticker = make(serde_json::json!({
            "type": "m.sticker",
            "event_id": "$stk:example.org",
            "sender": "@alice:example.org",
            "origin_server_ts": 1004,
            "content": {
                "body": "wave",
                "url": "mxc://example.org/abc",
                "info": {},
                "m.relates_to": {
                    "rel_type": "m.thread",
                    "event_id": "$root2:example.org"
                }
            }
        }));
        assert_eq!(
            extract_thread_root(&threaded_sticker).as_deref(),
            Some("$root2:example.org")
        );
    }

    #[test]
    fn prune_search_dir_removes_only_non_joined_room_dirs() {
        let dir = std::env::temp_dir().join(format!(
            "matrix-rat-prune-search-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let kept_dir = dir.join("!joined:example.org");
        let left_dir = dir.join("!left:example.org");
        let other_dir = dir.join("not-a-room-id");
        let stray_file = dir.join("!strayfile:example.org");
        std::fs::create_dir_all(&kept_dir).unwrap();
        std::fs::create_dir_all(&left_dir).unwrap();
        std::fs::create_dir_all(&other_dir).unwrap();
        std::fs::write(left_dir.join("seg"), b"data").unwrap();
        std::fs::write(&stray_file, b"keep").unwrap();

        let joined = HashSet::from(["!joined:example.org".to_owned()]);
        prune_search_dir(&dir, &joined);

        assert!(kept_dir.exists(), "joined room index must remain");
        assert!(!left_dir.exists(), "left room index must be pruned");
        assert!(other_dir.exists(), "non-room directory must not be touched");
        assert!(
            stray_file.exists(),
            "non-directory files must not be removed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn walk_contains(dir: &std::path::Path, needle: &[u8]) -> Option<std::path::PathBuf> {
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = walk_contains(&path, needle) {
                    return Some(found);
                }
            } else if let Ok(bytes) = std::fs::read(&path)
                && bytes.windows(needle.len()).any(|w| w == needle)
            {
                return Some(path);
            }
        }
        None
    }

    async fn run_search_cmd(
        handle: &MatrixHandle,
        events: &mut UnboundedReceiver<MatrixEvent>,
        generation: u64,
        query: &str,
    ) -> (Vec<SearchHit>, Option<String>) {
        handle.send(MatrixCmd::SearchMessages {
            generation,
            query: query.to_owned(),
        });
        let mut all = Vec::new();
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::SearchResults {
                        generation: got,
                        hits,
                        done,
                        note,
                    } if got == generation => {
                        all.extend(hits);
                        if done {
                            return (all, note);
                        }
                    }
                    MatrixEvent::Error(err) | MatrixEvent::LoginFailed(err) => {
                        panic!("worker error during search: {err}");
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(15), wait)
            .await
            .expect("search timed out")
    }

    async fn poll_search_until(
        handle: &MatrixHandle,
        events: &mut UnboundedReceiver<MatrixEvent>,
        gen_counter: &mut u64,
        query: &str,
        what: &str,
        mut pred: impl FnMut(&[SearchHit]) -> bool,
    ) -> Vec<SearchHit> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            *gen_counter += 1;
            let (hits, _note) = run_search_cmd(handle, events, *gen_counter, query).await;
            if pred(&hits) {
                return hits;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for search condition: {what} (last hits: {hits:?})");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn register_account(homeserver: &str, name: &str, password: &str, token: &str) {
        use matrix_sdk::ruma::api::client::account::register::v3::Request as RegisterRequest;
        let client = Client::builder()
            .homeserver_url(homeserver)
            .build()
            .await
            .expect("client");
        let mut request = RegisterRequest::new();
        request.username = Some(name.to_owned());
        request.password = Some(password.to_owned());
        request.inhibit_login = true;
        let session = match client.matrix_auth().register(request.clone()).await {
            Ok(_) => return,
            Err(err) => err
                .as_uiaa_response()
                .unwrap_or_else(|| panic!("registration failed: {err}"))
                .session
                .clone(),
        };
        let mut auth = RegistrationToken::new(token.to_owned());
        auth.session = session;
        request.auth = Some(AuthData::RegistrationToken(auth));
        client
            .matrix_auth()
            .register(request)
            .await
            .expect("registration with token");
    }

    /// STEP ZERO functional spike against a local homeserver (`--ignored --test-threads=1`):
    /// verifies live plaintext + encrypted indexing with `Previews(false)`, back-pagination,
    /// late decryption after verification/backup unlock, edits, redactions, restart
    /// persistence, on-disk encryption, and logout cleanup.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn global_search_spike_encrypted_edits_redactions_and_restart() {
        use futures::pin_mut;
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::encryption::verification::{SasState, VerificationRequestState};
        use matrix_sdk::ruma::api::client::uiaa::{MatrixUserIdentifier, Password, UserIdentifier};
        use matrix_sdk::ruma::events::key::verification::request::ToDeviceKeyVerificationRequestEvent;
        use tokio::sync::mpsc::unbounded_channel;

        let _ = tracing_subscriber::fmt()
            .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
            .with_test_writer()
            .try_init();
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let token = std::env::var("MATRIX_RAT_TEST_REGISTRATION_TOKEN")
            .unwrap_or_else(|_| "devtoken".to_owned());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let username = format!("ratsearch{stamp}");
        let password = format!("pw-{stamp}");
        register_account(&homeserver, &username, &password, &token).await;

        // Device A (plain SDK client): cross-signing + key backup + history before B exists.
        let a = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client A");
        a.matrix_auth()
            .login_username(&username, &password)
            .send()
            .await
            .expect("login A");
        a.sync_once(SyncSettings::default()).await.expect("sync A");
        if let Some(info) = a
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .err()
            .and_then(|e| e.as_uiaa_response().cloned())
        {
            let uid = a.user_id().expect("user_id").to_string();
            let mut uiaa = Password::new(
                UserIdentifier::Matrix(MatrixUserIdentifier::new(uid)),
                password.clone(),
            );
            uiaa.session = info.session;
            a.encryption()
                .bootstrap_cross_signing(Some(AuthData::Password(uiaa)))
                .await
                .expect("bootstrap cross-signing with password");
        }
        a.encryption()
            .recovery()
            .enable()
            .await
            .expect("recovery on A");

        // Room 1: plaintext with >25 old messages so initial sync misses the oldest one.
        let plain_room = a.create_room(CreateRoom::new()).await.expect("plain room");
        let plain_room_id = plain_room.room_id().to_string();
        let old_needle = format!("backpaginated_spike_{stamp}");
        plain_room
            .send(RoomMessageEventContent::text_plain(&old_needle))
            .await
            .expect("old plain");
        for i in 0..25 {
            plain_room
                .send(RoomMessageEventContent::text_plain(format!(
                    "filler_plain_{i}_{stamp}"
                )))
                .await
                .expect("filler plain");
        }

        // Room 2: E2E encrypted room with a pre-B message stored in key backup (initially UTD for B).
        let mut create_enc = CreateRoom::new();
        create_enc.initial_state = vec![
            InitialStateEvent::with_empty_state_key(
                RoomEncryptionEventContent::with_recommended_defaults(),
            )
            .to_raw_any(),
        ];
        let enc_room = a.create_room(create_enc).await.expect("enc room");
        let enc_room_id = enc_room.room_id().to_string();
        let utd_needle = format!("utd_unlock_spike_{stamp}");
        enc_room
            .send(RoomMessageEventContent::text_plain(&utd_needle))
            .await
            .expect("utd enc");
        let utd_thread_root_text = format!("utd_thread_root_spike_{stamp}");
        let utd_thread_reply_text = format!("utd_thread_reply_spike_{stamp}");
        let utd_thread_root_ev = enc_room
            .send(RoomMessageEventContent::text_plain(&utd_thread_root_text))
            .await
            .expect("utd thread root");
        let utd_thread_root_id = utd_thread_root_ev.response.event_id.to_string();
        let mut utd_thread_reply_content =
            RoomMessageEventContent::text_plain(&utd_thread_reply_text);
        utd_thread_reply_content.relates_to = Some(Relation::Thread(
            matrix_sdk::ruma::events::relation::Thread::plain(
                utd_thread_root_ev.response.event_id.clone(),
                utd_thread_root_ev.response.event_id.clone(),
            ),
        ));
        let utd_thread_reply_ev = enc_room
            .send(utd_thread_reply_content)
            .await
            .expect("utd thread reply");
        let utd_thread_reply_id = utd_thread_reply_ev.response.event_id.to_string();
        a.encryption()
            .backups()
            .wait_for_steady_state()
            .await
            .expect("backup steady state");

        // A stays online to answer SAS verification for B.
        let syncing_a = tokio::spawn({
            let a = a.clone();
            async move { a.sync(SyncSettings::default()).await }
        });
        let answering_a = tokio::spawn({
            let a = a.clone();
            async move {
                let (tx, mut rx) = unbounded_channel();
                a.add_event_handler(move |ev: ToDeviceKeyVerificationRequestEvent| {
                    let tx = tx.clone();
                    async move {
                        let _ = tx.send((ev.sender, ev.content.transaction_id.to_string()));
                    }
                });
                let (user_id, flow_id) = rx.recv().await.expect("verification request");
                let req = a
                    .encryption()
                    .get_verification_request(&user_id, &flow_id)
                    .await
                    .expect("known request");
                let changes = req.changes();
                pin_mut!(changes);
                req.accept().await.expect("accept req");
                let sas = loop {
                    if let VerificationRequestState::Transitioned { verification } = req.state()
                        && let Some(sas) = verification.sas()
                    {
                        break sas;
                    }
                    changes.next().await.expect("req change");
                };
                let sas_changes = sas.changes();
                pin_mut!(sas_changes);
                sas.accept().await.expect("accept sas");
                loop {
                    match sas.state() {
                        SasState::KeysExchanged { .. } => sas.confirm().await.expect("confirm sas"),
                        SasState::Done { .. } => return,
                        SasState::Cancelled(info) => panic!("A sas cancelled: {}", info.reason()),
                        _ => {}
                    }
                    sas_changes.next().await.expect("sas change");
                }
            }
        });

        // Device B (matrix-rat worker) with Previews(false).
        let root = std::env::temp_dir().join(format!("matrix-rat-search-spike-{stamp}"));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "search-spike".into(),
        };
        paths.create_dirs().unwrap();
        let (b, mut events) = spawn(paths.clone());
        b.send(MatrixCmd::LoginPassword {
            homeserver: homeserver.clone(),
            username: username.clone(),
            password: password.clone(),
        });
        b.send(MatrixCmd::Previews(false));

        let mut search_gen = 0u64;
        // Wait until B sees both rooms and knows of device A.
        // Wait for A to also see B below before sending live encrypted messages.
        let (mut saw_plain, mut saw_enc, mut saw_a) = (false, false, false);
        tokio::time::timeout(Duration::from_secs(45), async {
            while !(saw_plain && saw_enc && saw_a) {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Rooms(rooms) => {
                        saw_plain |= rooms.iter().any(|r| r.id == plain_room_id);
                        saw_enc |= rooms.iter().any(|r| r.id == enc_room_id);
                    }
                    MatrixEvent::Security(sec) => saw_a = sec.other_devices,
                    MatrixEvent::LoginFailed(e) | MatrixEvent::Error(e) => panic!("{e}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out waiting for initial rooms");

        // Wait until A sees B's device so live E2E messages encrypt for B.
        tokio::time::timeout(Duration::from_secs(30), async {
            let uid = a.user_id().unwrap().to_owned();
            while a
                .encryption()
                .get_user_devices(&uid)
                .await
                .unwrap()
                .devices()
                .count()
                < 2
            {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("A never saw B");

        // 1. Live messages with Previews(false) in both plaintext and encrypted rooms.
        let live_plain = format!("live_plain_spike_{stamp}");
        let live_enc = format!("live_e2e_spike_{stamp}");
        let edit_before = format!("edit_before_spike_{stamp}");
        let edit_after = format!("edit_after_spike_{stamp}");
        let redact_needle = format!("redact_spike_{stamp}");
        let thread_root_text = format!("thread_root_spike_{stamp}");
        let thread_reply_text = format!("thread_reply_spike_{stamp}");

        plain_room
            .send(RoomMessageEventContent::text_plain(&live_plain))
            .await
            .expect("send live plain");
        enc_room
            .send(RoomMessageEventContent::text_plain(&live_enc))
            .await
            .expect("send live enc");
        let edited_ev = enc_room
            .send(RoomMessageEventContent::text_plain(&edit_before))
            .await
            .expect("send edit target");
        let thread_root_ev = enc_room
            .send(RoomMessageEventContent::text_plain(&thread_root_text))
            .await
            .expect("send thread root");
        let thread_root_id = thread_root_ev.response.event_id.to_string();
        let mut thread_reply_content = RoomMessageEventContent::text_plain(&thread_reply_text);
        thread_reply_content.relates_to = Some(Relation::Thread(
            matrix_sdk::ruma::events::relation::Thread::plain(
                thread_root_ev.response.event_id.clone(),
                thread_root_ev.response.event_id.clone(),
            ),
        ));
        let thread_reply_ev = enc_room
            .send(thread_reply_content)
            .await
            .expect("send thread reply");
        let thread_reply_id = thread_reply_ev.response.event_id.to_string();
        let red_ev = plain_room
            .send(RoomMessageEventContent::text_plain(&redact_needle))
            .await
            .expect("send redact target");

        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &live_plain,
            "live plain",
            |h| {
                h.iter()
                    .any(|x| x.room_id == plain_room_id && x.snippet.contains(&live_plain))
            },
        )
        .await;
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &live_enc,
            "live enc",
            |h| {
                h.iter().any(|x| {
                    x.room_id == enc_room_id
                        && x.thread_root.is_none()
                        && x.snippet.contains(&live_enc)
                })
            },
        )
        .await;
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &thread_reply_text,
            "encrypted thread reply",
            |h| {
                h.iter().any(|x| {
                    x.room_id == enc_room_id
                        && x.event_id == thread_reply_id
                        && x.thread_root.as_deref() == Some(thread_root_id.as_str())
                        && x.snippet.contains(&thread_reply_text)
                })
            },
        )
        .await;
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &redact_needle,
            "redact target",
            |h| !h.is_empty(),
        )
        .await;

        // UTD messages should NOT be searchable while locked.
        search_gen += 1;
        let (utd_before, _) = run_search_cmd(&b, &mut events, search_gen, &utd_needle).await;
        assert!(
            utd_before.is_empty(),
            "locked UTD message must not match before verification: {utd_before:?}"
        );
        search_gen += 1;
        let (utd_reply_before, _) =
            run_search_cmd(&b, &mut events, search_gen, &utd_thread_reply_text).await;
        assert!(
            utd_reply_before.is_empty(),
            "locked UTD thread reply must not match before verification: {utd_reply_before:?}"
        );

        // 2. Back-pagination indexes older history in plain_room.
        // Confirm old_needle isn't in the initial sync window first.
        search_gen += 1;
        let (before_page, _) = run_search_cmd(&b, &mut events, search_gen, &old_needle).await;
        assert!(
            before_page.is_empty(),
            "old message beyond initial sync limit unexpectedly present before pagination"
        );
        b.send(MatrixCmd::OpenRoom {
            room_id: plain_room_id.clone(),
            generation: 10,
        });
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut paginating = false;
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Timeline {
                        room_id,
                        reached_start,
                        messages,
                        ..
                    } if room_id == plain_room_id => {
                        if messages.iter().any(|m| m.body.contains(&old_needle)) || reached_start {
                            break;
                        }
                        if !paginating {
                            paginating = true;
                            b.send(MatrixCmd::Paginate {
                                room_id: plain_room_id.clone(),
                                generation: 10,
                            });
                        }
                    }
                    MatrixEvent::Paginated {
                        generation: 10,
                        reached_start,
                    } => {
                        if reached_start {
                            break;
                        }
                        paginating = false;
                        b.send(MatrixCmd::Paginate {
                            room_id: plain_room_id.clone(),
                            generation: 10,
                        });
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out paginating old plain message");
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &old_needle,
            "back-paginated",
            |h| {
                h.iter()
                    .any(|x| x.room_id == plain_room_id && x.snippet.contains(&old_needle))
            },
        )
        .await;

        // 3. Edit updates index (old text gone, new text present + snippet updated).
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &edit_before,
            "edit before",
            |h| !h.is_empty(),
        )
        .await;
        let replacement = RoomMessageEventContent::text_plain(&edit_after).make_replacement(
            matrix_sdk::ruma::events::room::message::ReplacementMetadata::new(
                edited_ev.response.event_id.clone(),
                None,
            ),
        );
        enc_room.send(replacement).await.expect("send replacement");
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &edit_after,
            "edit after",
            |h| {
                h.iter()
                    .any(|x| x.room_id == enc_room_id && x.snippet.contains(&edit_after))
            },
        )
        .await;
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &edit_before,
            "old edited text removed",
            |h| h.is_empty(),
        )
        .await;

        // 4. Redaction removes from index.
        plain_room
            .redact(&red_ev.response.event_id, None, None)
            .await
            .expect("redact");
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &redact_needle,
            "redacted text removed",
            |h| h.is_empty(),
        )
        .await;

        // 5. Open encrypted room, verify B with A -> backup unlocks -> late decryption indexes UTD.
        b.send(MatrixCmd::OpenRoom {
            room_id: enc_room_id.clone(),
            generation: 20,
        });
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let MatrixEvent::Timeline {
                    room_id, messages, ..
                } = events.recv().await.expect("worker stopped")
                    && room_id == enc_room_id
                    && messages.iter().any(|m| m.kind == MsgKind::Undecryptable)
                {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for UTD in enc room");

        // Wait until A sees B's device before requesting SAS.
        tokio::time::timeout(Duration::from_secs(30), async {
            let uid = a.user_id().unwrap().to_owned();
            while a
                .encryption()
                .get_user_devices(&uid)
                .await
                .unwrap()
                .devices()
                .count()
                < 2
            {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("A never saw B");

        b.send(MatrixCmd::Verification(crate::matrix::VerifyCmd::Request));
        let (mut verified, mut backup_enabled, mut unlocked) = (false, false, false);
        tokio::time::timeout(Duration::from_secs(60), async {
            use crate::matrix::{BackupStatus, DeviceTrust, VerifyCmd, VerifyStage};
            let mut confirmed = false;
            while !(verified && backup_enabled && unlocked) {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Verification(u) => match u.stage {
                        VerifyStage::Requested => {
                            b.send(MatrixCmd::Verification(VerifyCmd::Answer {
                                flow: u.flow,
                                answer: crate::matrix::Answer::Accept,
                            }));
                        }
                        VerifyStage::Compare { .. } if !confirmed => {
                            confirmed = true;
                            b.send(MatrixCmd::Verification(VerifyCmd::Answer {
                                flow: u.flow,
                                answer: crate::matrix::Answer::Match,
                            }));
                        }
                        VerifyStage::Cancelled { reason } => panic!("B cancelled: {reason}"),
                        _ => {}
                    },
                    MatrixEvent::Security(s) => {
                        verified = s.device == DeviceTrust::Verified;
                        backup_enabled = s.backup == BackupStatus::Enabled;
                    }
                    MatrixEvent::Timeline {
                        room_id, messages, ..
                    } if room_id == enc_room_id => {
                        unlocked = messages
                            .iter()
                            .any(|m| m.body == utd_needle && m.kind != MsgKind::Undecryptable);
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("timed out verifying & decrypting UTD");
        tokio::time::timeout(Duration::from_secs(10), answering_a)
            .await
            .expect("A answer timeout")
            .expect("A answer join");

        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &utd_needle,
            "late decrypted UTD",
            |h| {
                h.iter()
                    .any(|x| x.room_id == enc_room_id && x.snippet.contains(&utd_needle))
            },
        )
        .await;
        poll_search_until(
            &b,
            &mut events,
            &mut search_gen,
            &utd_thread_reply_text,
            "late decrypted UTD thread reply",
            |h| {
                h.iter().any(|x| {
                    x.room_id == enc_room_id
                        && x.event_id == utd_thread_reply_id
                        && x.thread_root.as_deref() == Some(utd_thread_root_id.as_str())
                        && x.snippet.contains(&utd_thread_reply_text)
                })
            },
        )
        .await;

        // 6. On-disk search index files contain no plaintext needles.
        for needle in [
            &live_plain,
            &live_enc,
            &old_needle,
            &utd_needle,
            &utd_thread_reply_text,
            &edit_after,
            &thread_reply_text,
        ] {
            if let Some(leaked) = walk_contains(&paths.search_dir(), needle.as_bytes()) {
                panic!(
                    "plaintext search term leaked to disk in {}",
                    leaked.display()
                );
            }
        }

        // 7. Shutdown and restart: persisted encrypted index reopens with offline_client.
        b.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}

        let (b2, mut events2) = spawn(paths.clone());
        let mut gen2 = 0u64;
        tokio::time::timeout(Duration::from_secs(30), async {
            let (mut p, mut e) = (false, false);
            while !(p && e) {
                if let MatrixEvent::Rooms(rooms) = events2.recv().await.expect("worker2 stopped") {
                    p |= rooms.iter().any(|r| r.id == plain_room_id);
                    e |= rooms.iter().any(|r| r.id == enc_room_id);
                }
            }
        })
        .await
        .expect("timed out restoring rooms on restart");

        for needle in [
            &live_plain,
            &live_enc,
            &old_needle,
            &utd_needle,
            &edit_after,
            &thread_reply_text,
        ] {
            poll_search_until(
                &b2,
                &mut events2,
                &mut gen2,
                needle,
                "persisted after restart",
                |h| h.iter().any(|x| x.snippet.contains(needle)),
            )
            .await;
        }
        let edited_id_str = edited_ev.response.event_id.to_string();
        let (edited_hits, _) = run_search_cmd(&b2, &mut events2, gen2 + 1, &edit_after).await;
        gen2 += 1;
        assert!(
            edited_hits
                .iter()
                .any(|x| x.room_id == enc_room_id && x.event_id == edited_id_str),
            "edited search hit must carry original event_id {edited_id_str}: {edited_hits:?}"
        );
        let (thr_hits, _) = run_search_cmd(&b2, &mut events2, gen2 + 1, &thread_reply_text).await;
        gen2 += 1;
        assert!(
            thr_hits.iter().any(|x| {
                x.room_id == enc_room_id
                    && x.event_id == thread_reply_id
                    && x.thread_root.as_deref() == Some(thread_root_id.as_str())
            }),
            "restarted search must preserve thread_root {thread_root_id}: {thr_hits:?}"
        );

        // 7b. Leaving a room excludes its hits immediately and prunes its index directory.
        b2.send(MatrixCmd::Leave {
            room_id: plain_room_id.clone(),
        });
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let MatrixEvent::Rooms(rooms) = events2.recv().await.expect("worker2 stopped")
                    && !rooms.iter().any(|r| r.id == plain_room_id)
                {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for left room to leave room list");
        gen2 += 1;
        let (left_hits, _) = run_search_cmd(&b2, &mut events2, gen2, &live_plain).await;
        assert!(
            left_hits.is_empty(),
            "left room must not return search hits: {left_hits:?}"
        );
        b2.send(MatrixCmd::Shutdown);
        while events2.recv().await.is_some() {}
        assert!(
            !paths.search_dir().join(&plain_room_id).exists(),
            "left room index directory must be pruned on shutdown"
        );
        assert!(
            paths.search_dir().join(&enc_room_id).exists(),
            "joined room index directory must remain"
        );

        // 8. Logout removes search_dir() completely.
        let (b3, mut events3) = spawn(paths.clone());
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let MatrixEvent::Rooms(rooms) = events3.recv().await.expect("worker3 stopped")
                    && rooms.iter().any(|r| r.id == enc_room_id)
                {
                    break;
                }
            }
        })
        .await
        .expect("timed out restoring worker3 before logout");
        b3.send(MatrixCmd::Logout);
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if matches!(
                    events3.recv().await.expect("worker3 stopped"),
                    MatrixEvent::NeedLogin { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("timed out waiting for NeedLogin after Logout");
        assert!(
            !paths.search_dir().exists(),
            "search_dir should be removed on Logout"
        );
        assert!(
            !paths.store_dir().exists(),
            "store_dir should be removed on Logout"
        );

        b3.send(MatrixCmd::Shutdown);
        while events3.recv().await.is_some() {}
        syncing_a.abort();
        let _ = std::fs::remove_dir_all(root);
    }
}
