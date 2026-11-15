//! MSC2545 / Matrix 1.19 image packs (`im.ponies.*` and `m.image_pack.*` / `m.room.image_pack`)
//! for custom emoji and stickers, loaded lazily on first use and cached for the session.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::ruma::RoomId;
use matrix_sdk::ruma::api::client::state::get_state_events;
use matrix_sdk::ruma::events::{GlobalAccountDataEventType, StateEventType};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, mpsc::UnboundedSender};

use super::MatrixEvent;
use crate::text::sanitize_line;

const MAX_PACK_ROOMS: usize = 8;
const STATE_FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// A custom emoji from an MSC2545 / Matrix image pack.
#[derive(Clone, PartialEq, Eq)]
pub struct CustomEmoji {
    pub shortcode: String,
    pub url: String,
    pub body: Option<String>,
    pub pack_name: String,
}

impl fmt::Debug for CustomEmoji {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustomEmoji").finish_non_exhaustive()
    }
}

/// A sticker from an MSC2545 / Matrix image pack.
#[derive(Clone, PartialEq, Eq)]
pub struct StickerItem {
    pub shortcode: String,
    pub name: String,
    pub url: String,
    pub pack_name: String,
    pub dimensions: Option<(u32, u32)>,
    pub size: Option<u64>,
    pub mime: Option<String>,
}

impl fmt::Debug for StickerItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StickerItem")
            .field("dimensions", &self.dimensions)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// One loaded image pack (containing custom emoji, stickers, or both).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct EmotePack {
    pub id: String,
    pub name: String,
    pub emojis: Vec<CustomEmoji>,
    pub stickers: Vec<StickerItem>,
}

impl fmt::Debug for EmotePack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmotePack")
            .field("emojis", &self.emojis.len())
            .field("stickers", &self.stickers.len())
            .finish_non_exhaustive()
    }
}

/// Session-cached catalog of custom emoji and sticker packs active for the user and current room.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct EmoteCatalog {
    pub packs: Vec<EmotePack>,
}

impl fmt::Debug for EmoteCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmoteCatalog")
            .field("packs", &self.packs.len())
            .finish()
    }
}

impl EmoteCatalog {
    pub fn is_empty(&self) -> bool {
        self.packs.is_empty()
    }

    pub fn emoji_packs(&self) -> impl Iterator<Item = &EmotePack> {
        self.packs.iter().filter(|p| !p.emojis.is_empty())
    }

    pub fn sticker_packs(&self) -> impl Iterator<Item = &EmotePack> {
        self.packs.iter().filter(|p| !p.stickers.is_empty())
    }

    /// Deduplicated custom emoji across all active packs, preserving pack priority order.
    pub fn all_emojis(&self) -> Vec<CustomEmoji> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for pack in &self.packs {
            for emoji in &pack.emojis {
                if seen.insert(emoji.shortcode.clone()) {
                    out.push(emoji.clone());
                }
            }
        }
        out
    }

    pub fn shortcode_for_url(&self, url: &str) -> Option<&str> {
        for pack in &self.packs {
            for emoji in &pack.emojis {
                if emoji.url == url {
                    return Some(&emoji.shortcode);
                }
            }
        }
        None
    }

    pub fn url_for_shortcode(&self, code: &str) -> Option<&str> {
        let bare = code.trim().trim_matches(':');
        for pack in &self.packs {
            for emoji in &pack.emojis {
                if emoji.shortcode.eq_ignore_ascii_case(bare) {
                    return Some(&emoji.url);
                }
            }
        }
        None
    }
}

pub fn is_valid_mxc(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("mxc://") else {
        return false;
    };
    let Some((server, media_id)) = rest.split_once('/') else {
        return false;
    };
    !server.is_empty()
        && !media_id.is_empty()
        && !url.contains(|c: char| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>' | '"'))
}

fn clean_shortcode(raw: &str) -> Option<String> {
    let trimmed = sanitize_line(raw.trim().trim_matches(':'));
    if trimmed.is_empty()
        || trimmed.len() > 64
        || trimmed.contains(|c: char| c.is_whitespace() || c == ':')
    {
        return None;
    }
    Some(trimmed)
}

fn parse_usage(arr: Option<&Vec<Value>>, default: (bool, bool)) -> (bool, bool) {
    let Some(items) = arr else {
        return default;
    };
    if items.is_empty() {
        return default;
    }
    let mut emoticon = false;
    let mut sticker = false;
    for item in items {
        match item.as_str() {
            Some("emoticon") => emoticon = true,
            Some("sticker") => sticker = true,
            _ => {}
        }
    }
    if !emoticon && !sticker {
        default
    } else {
        (emoticon, sticker)
    }
}

/// Parses a single MSC2545 / Matrix 1.19 image pack JSON object (`im.ponies.user_emotes`,
/// `im.ponies.room_emotes`, or `m.room.image_pack`).
pub fn parse_pack_json(raw: &Value, default_name: &str, pack_id: &str) -> Option<EmotePack> {
    let pack_obj = raw.get("pack").and_then(Value::as_object);
    let pack_name = pack_obj
        .and_then(|p| {
            p.get("display_name")
                .or_else(|| p.get("name"))
                .and_then(Value::as_str)
        })
        .map(sanitize_line)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| sanitize_line(default_name));
    let pack_usage = parse_usage(
        pack_obj.and_then(|p| p.get("usage")).and_then(Value::as_array),
        (true, true),
    );

    let mut emojis = Vec::new();
    let mut stickers = Vec::new();

    if let Some(images) = raw.get("images").and_then(Value::as_object) {
        for (raw_key, entry) in images {
            let Some(shortcode) = clean_shortcode(raw_key) else {
                continue;
            };
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let Some(url) = obj.get("url").and_then(Value::as_str).filter(|u| is_valid_mxc(u)) else {
                continue;
            };
            let body = obj
                .get("body")
                .and_then(Value::as_str)
                .map(sanitize_line)
                .filter(|s| !s.is_empty());
            let (is_emote, is_sticker) = parse_usage(
                obj.get("usage").and_then(Value::as_array),
                pack_usage,
            );
            let info = obj.get("info").and_then(Value::as_object);
            let dimensions = info.and_then(|inf| {
                let w = u32::try_from(inf.get("w")?.as_u64()?).ok()?;
                let h = u32::try_from(inf.get("h")?.as_u64()?).ok()?;
                (w > 0 && h > 0).then_some((w, h))
            });
            let size = info.and_then(|inf| inf.get("size")?.as_u64());
            let mime = info
                .and_then(|inf| inf.get("mimetype")?.as_str())
                .map(sanitize_line)
                .filter(|s| !s.is_empty());

            if is_emote {
                emojis.push(CustomEmoji {
                    shortcode: shortcode.clone(),
                    url: url.to_owned(),
                    body: body.clone(),
                    pack_name: pack_name.clone(),
                });
            }
            if is_sticker {
                stickers.push(StickerItem {
                    name: body.unwrap_or_else(|| shortcode.clone()),
                    shortcode,
                    url: url.to_owned(),
                    pack_name: pack_name.clone(),
                    dimensions,
                    size,
                    mime,
                });
            }
        }
    }

    // Legacy `im.ponies.user_emotes` `shortcodes: { "slug": "mxc://..." }` compatibility.
    if emojis.is_empty()
        && let Some(shortcodes) = raw.get("shortcodes").and_then(Value::as_object)
    {
        for (raw_key, val) in shortcodes {
            let Some(shortcode) = clean_shortcode(raw_key) else {
                continue;
            };
            let url_opt = val
                .as_str()
                .or_else(|| val.get("url").and_then(Value::as_str));
            let Some(url) = url_opt.filter(|u| is_valid_mxc(u)) else {
                continue;
            };
            emojis.push(CustomEmoji {
                shortcode,
                url: url.to_owned(),
                body: None,
                pack_name: pack_name.clone(),
            });
        }
    }

    if emojis.is_empty() && stickers.is_empty() {
        return None;
    }

    Some(EmotePack {
        id: pack_id.to_owned(),
        name: pack_name,
        emojis,
        stickers,
    })
}

/// Parses `im.ponies.emote_rooms` or `m.image_pack.rooms` global account data into
/// `(room_id, state_keys)` pairs.
pub fn parse_emote_rooms_json(raw: &Value) -> Vec<(String, Vec<String>)> {
    let Some(rooms) = raw.get("rooms").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (room_id, keys_val) in rooms {
        if room_id.is_empty() {
            continue;
        }
        let keys: Vec<String> = keys_val
            .as_object()
            .map(|obj| obj.keys().cloned().collect())
            .unwrap_or_default();
        out.push((room_id.clone(), keys));
    }
    out
}

#[derive(Default)]
struct CacheState {
    /// True only after the UI has asked for emote packs at least once in this session.
    requested: bool,
    active_room: Option<String>,
    user_pack: Option<Option<EmotePack>>,
    emote_rooms: Option<Vec<(String, Vec<String>)>>,
    room_packs: HashMap<String, Vec<(String, EmotePack)>>,
}

/// Lazily loads and caches MSC2545 / Matrix 1.19 image packs for the session.
/// Makes zero startup HTTP requests until [`EmoteManager::ensure`] is called.
pub(super) struct EmoteManager {
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    state: Arc<Mutex<CacheState>>,
    poke: Arc<Notify>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl EmoteManager {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        let state = Arc::new(Mutex::new(CacheState::default()));
        let poke = Arc::new(Notify::new());
        let task_client = client.clone();
        let task_events = events.clone();
        let task_state = state.clone();
        let task_poke = poke.clone();
        let task = tokio::spawn(async move {
            loop {
                task_poke.notified().await;
                let (requested, active_room) = {
                    let guard = task_state.lock().await;
                    (guard.requested, guard.active_room.clone())
                };
                if !requested {
                    continue;
                }
                let catalog =
                    load_catalog(&task_client, &task_state, active_room.as_deref()).await;
                let _ = task_events.send(MatrixEvent::EmotePacks(catalog));
            }
        });
        Self {
            client,
            events,
            state,
            poke,
            task: Some(task),
        }
    }

    pub fn ensure(&self, room_id: Option<String>) {
        let state = self.state.clone();
        let poke = self.poke.clone();
        tokio::spawn(async move {
            let mut guard = state.lock().await;
            guard.requested = true;
            if room_id.is_some() {
                guard.active_room = room_id;
            }
            drop(guard);
            poke.notify_one();
        });
    }

    pub fn register_handlers(
        &self,
        client: &Client,
    ) -> [matrix_sdk::event_handler::EventHandlerHandle; 2] {
        let state_global = self.state.clone();
        let poke_global = self.poke.clone();
        let global_handle = client.add_event_handler(
            move |ev: matrix_sdk::ruma::serde::Raw<
                matrix_sdk::ruma::events::AnyGlobalAccountDataEvent,
            >| {
                let state = state_global.clone();
                let poke = poke_global.clone();
                async move {
                    let Ok(val) = serde_json::from_str::<Value>(ev.json().get()) else {
                        return;
                    };
                    let Some(ev_type) = val.get("type").and_then(Value::as_str) else {
                        return;
                    };
                    let is_user = ev_type == "im.ponies.user_emotes";
                    let is_rooms =
                        ev_type == "im.ponies.emote_rooms" || ev_type == "m.image_pack.rooms";
                    if !is_user && !is_rooms {
                        return;
                    }
                    let mut guard = state.lock().await;
                    if is_user {
                        guard.user_pack = None;
                    }
                    if is_rooms {
                        guard.emote_rooms = None;
                    }
                    let should_poke = guard.requested;
                    drop(guard);
                    if should_poke {
                        poke.notify_one();
                    }
                }
            },
        );

        let state_room = self.state.clone();
        let poke_room = self.poke.clone();
        let room_handle = client.add_event_handler(
            move |ev: matrix_sdk::ruma::serde::Raw<matrix_sdk::ruma::events::AnySyncStateEvent>,
                  room: matrix_sdk::Room| {
                let state = state_room.clone();
                let poke = poke_room.clone();
                async move {
                    let Ok(val) = serde_json::from_str::<Value>(ev.json().get()) else {
                        return;
                    };
                    let Some(ev_type) = val.get("type").and_then(Value::as_str) else {
                        return;
                    };
                    if ev_type != "im.ponies.room_emotes" && ev_type != "m.room.image_pack" {
                        return;
                    }
                    let mut guard = state.lock().await;
                    guard.room_packs.remove(room.room_id().as_str());
                    let should_poke = guard.requested;
                    drop(guard);
                    if should_poke {
                        poke.notify_one();
                    }
                }
            },
        );

        [global_handle, room_handle]
    }

    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = (&self.client, &self.events);
    }
}

async fn fetch_global_json(client: &Client, event_type: &str) -> Option<Value> {
    let ty = GlobalAccountDataEventType::from(event_type);
    if let Ok(Some(raw)) = client.account().account_data_raw(ty.clone()).await
        && let Ok(val) = serde_json::from_str::<Value>(raw.json().get())
    {
        return Some(val.get("content").cloned().unwrap_or(val));
    }
    if let Ok(Some(raw)) = client.account().fetch_account_data(ty).await
        && let Ok(val) = serde_json::from_str::<Value>(raw.json().get())
    {
        return Some(val.get("content").cloned().unwrap_or(val));
    }
    None
}

async fn load_room_packs(client: &Client, room_id_str: &str) -> Vec<(String, EmotePack)> {
    let Ok(room_id) = RoomId::parse(room_id_str) else {
        return Vec::new();
    };
    let Some(room) = client.get_room(&room_id) else {
        return Vec::new();
    };
    let room_name = room
        .name()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Room pack".to_owned());

    let mut out = Vec::new();
    let mut seen_keys = HashSet::new();

    // 1. Check local state store first (zero HTTP requests when already synced).
    for ev_type in [
        StateEventType::from("im.ponies.room_emotes"),
        StateEventType::RoomImagePack,
    ] {
        if let Ok(events) = room.get_state_events(ev_type).await {
            for raw in events {
                if let Ok(val) = serde_json::from_str::<Value>(raw.json().get()) {
                    let state_key = val
                        .get("state_key")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    if let Some(content) = val.get("content")
                        && seen_keys.insert(state_key.clone())
                    {
                        let pack_id = format!("{room_id_str}:{state_key}");
                        if let Some(pack) = parse_pack_json(content, &room_name, &pack_id) {
                            out.push((state_key, pack));
                        }
                    }
                }
            }
        }
    }
    if !out.is_empty() {
        return out;
    }

    // 2. Fallback to a single bounded `/state` fetch per room per session when unsynced.
    let req = get_state_events::v3::Request::new(room_id);
    if let Ok(Ok(resp)) = tokio::time::timeout(STATE_FETCH_TIMEOUT, client.send(req)).await {
        for raw in resp.room_state {
            if let Ok(val) = serde_json::from_str::<Value>(raw.json().get()) {
                let ev_type = val.get("type").and_then(Value::as_str).unwrap_or("");
                if ev_type != "im.ponies.room_emotes" && ev_type != "m.room.image_pack" {
                    continue;
                }
                let state_key = val
                    .get("state_key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                if let Some(content) = val.get("content")
                    && seen_keys.insert(state_key.clone())
                {
                    let pack_id = format!("{room_id_str}:{state_key}");
                    if let Some(pack) = parse_pack_json(content, &room_name, &pack_id) {
                        out.push((state_key, pack));
                    }
                }
            }
        }
    }
    out
}

async fn load_catalog(
    client: &Client,
    state: &Arc<Mutex<CacheState>>,
    active_room: Option<&str>,
) -> EmoteCatalog {
    let need_user = state.lock().await.user_pack.is_none();
    if need_user {
        let user_pack = fetch_global_json(client, "im.ponies.user_emotes")
            .await
            .and_then(|val| parse_pack_json(&val, "Personal", "user"));
        state.lock().await.user_pack = Some(user_pack);
    }

    let need_rooms = state.lock().await.emote_rooms.is_none();
    if need_rooms {
        let mut rooms = Vec::new();
        if let Some(val) = fetch_global_json(client, "im.ponies.emote_rooms").await {
            rooms.extend(parse_emote_rooms_json(&val));
        }
        if let Some(val) = fetch_global_json(client, "m.image_pack.rooms").await {
            for (rid, keys) in parse_emote_rooms_json(&val) {
                if let Some((_, existing)) = rooms.iter_mut().find(|(r, _)| r == &rid) {
                    for k in keys {
                        if !existing.contains(&k) {
                            existing.push(k);
                        }
                    }
                } else {
                    rooms.push((rid, keys));
                }
            }
        }
        rooms.truncate(MAX_PACK_ROOMS);
        state.lock().await.emote_rooms = Some(rooms);
    }

    let mut target_rooms: Vec<String> = Vec::new();
    if let Some(rid) = active_room {
        target_rooms.push(rid.to_owned());
    }
    {
        let guard = state.lock().await;
        if let Some(emote_rooms) = &guard.emote_rooms {
            for (rid, _) in emote_rooms {
                if !target_rooms.contains(rid) {
                    target_rooms.push(rid.clone());
                }
            }
        }
    }

    for rid in &target_rooms {
        let already = state.lock().await.room_packs.contains_key(rid);
        if !already {
            let loaded = load_room_packs(client, rid).await;
            state.lock().await.room_packs.insert(rid.clone(), loaded);
        }
    }

    let guard = state.lock().await;
    let mut packs = Vec::new();
    let mut seen_pack_ids = HashSet::new();

    if let Some(Some(user_pack)) = &guard.user_pack {
        seen_pack_ids.insert(user_pack.id.clone());
        packs.push(user_pack.clone());
    }

    if let Some(rid) = active_room
        && let Some(room_packs) = guard.room_packs.get(rid)
    {
        for (_, pack) in room_packs {
            if seen_pack_ids.insert(pack.id.clone()) {
                packs.push(pack.clone());
            }
        }
    }

    if let Some(emote_rooms) = &guard.emote_rooms {
        for (rid, keys) in emote_rooms {
            if let Some(room_packs) = guard.room_packs.get(rid) {
                for (skey, pack) in room_packs {
                    if (keys.is_empty() || keys.contains(skey))
                        && seen_pack_ids.insert(pack.id.clone())
                    {
                        packs.push(pack.clone());
                    }
                }
            }
        }
    }

    EmoteCatalog { packs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_msc2545_user_and_room_packs_and_redacts_debug() {
        let raw = json!({
            "pack": {
                "display_name": "Ferris & Friends",
                "usage": ["emoticon", "sticker"]
            },
            "images": {
                "crab_happy": {
                    "url": "mxc://example.org/crab1",
                    "body": "Happy Ferris",
                    "usage": ["emoticon"],
                    "info": { "w": 64, "h": 64, "size": 2048, "mimetype": "image/png" }
                },
                "crab_wave": {
                    "url": "mxc://example.org/crab2",
                    "body": "Waving Ferris",
                    "usage": ["sticker"],
                    "info": { "w": 256, "h": 256, "size": 12345, "mimetype": "image/webp" }
                },
                "crab_both": {
                    "url": "mxc://example.org/crab3"
                },
                "bad url": {
                    "url": "https://evil.example/x.png"
                }
            }
        });

        let pack = parse_pack_json(&raw, "Fallback", "room1:main").expect("valid pack");
        assert_eq!(pack.name, "Ferris & Friends");
        assert_eq!(pack.emojis.len(), 2);
        assert_eq!(pack.stickers.len(), 2);
        assert_eq!(pack.emojis[0].shortcode, "crab_both");
        assert_eq!(pack.emojis[1].shortcode, "crab_happy");
        assert_eq!(pack.stickers[1].shortcode, "crab_wave");
        assert_eq!(pack.stickers[1].dimensions, Some((256, 256)));
        assert_eq!(pack.stickers[1].size, Some(12345));

        let catalog = EmoteCatalog { packs: vec![pack] };
        assert_eq!(
            catalog.shortcode_for_url("mxc://example.org/crab1"),
            Some("crab_happy")
        );
        assert_eq!(
            catalog.url_for_shortcode(":crab_happy:"),
            Some("mxc://example.org/crab1")
        );

        let dbg = format!("{catalog:?} {:?} {:?}", catalog.packs[0].emojis[0], catalog.packs[0].stickers[0]);
        assert!(!dbg.contains("Ferris"), "debug leaked pack name: {dbg}");
        assert!(!dbg.contains("crab1"), "debug leaked mxc url: {dbg}");
        assert!(!dbg.contains("crab_happy"), "debug leaked shortcode: {dbg}");
    }

    #[test]
    fn parses_emote_rooms_and_legacy_shortcodes() {
        let rooms_json = json!({
            "rooms": {
                "!pack1:example.org": {
                    "ponies": {},
                    "default": {}
                }
            }
        });
        let rooms = parse_emote_rooms_json(&rooms_json);
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].0, "!pack1:example.org");
        assert_eq!(rooms[0].1.len(), 2);

        let legacy = json!({
            "shortcodes": {
                "party_blob": "mxc://example.org/blob1"
            }
        });
        let pack = parse_pack_json(&legacy, "Personal", "user").expect("legacy pack");
        assert_eq!(pack.emojis.len(), 1);
        assert_eq!(pack.emojis[0].shortcode, "party_blob");
        assert!(pack.stickers.is_empty());
    }
}
