//! Image packs: custom emoji and stickers (MSC2545, `m.room.image_pack` and
//! `m.image_pack.rooms` in Matrix 1.19, and the older `im.ponies.*` events). Loaded on
//! first use and kept for the session.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use matrix_sdk::Client;
use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState;
use matrix_sdk::ruma::api::client::state::get_state_event_for_key;
use matrix_sdk::ruma::events::{GlobalAccountDataEventType, StateEventType};
use matrix_sdk::ruma::{MxcUri, RoomId};
use serde_json::Value;
use tokio::sync::{Notify, mpsc::UnboundedSender};

use super::MatrixEvent;
use crate::text::{is_blank, sanitize_line, truncate};

/// Pack rooms named in the account data that are read; the rest are ignored.
const MAX_PACK_ROOMS: usize = 8;
/// Packs (state keys) read per room.
const MAX_PACKS_PER_ROOM: usize = 8;
/// Packs in the catalog in all.
const MAX_PACKS: usize = 24;
/// Images read per pack.
const MAX_IMAGES_PER_PACK: usize = 300;
/// Longest shortcode kept, in bytes.
pub const MAX_SHORTCODE_LEN: usize = 64;
/// Pack and sticker names are cut to this many columns.
const MAX_NAME_WIDTH: usize = 64;
const MAX_MXC_LEN: usize = 255;
const STATE_FETCH_TIMEOUT: Duration = Duration::from_secs(4);

/// The room pack event types, the stable one first: it wins over the older one when a
/// room has both under the same state key.
const ROOM_PACK_TYPES: [&str; 2] = ["m.room.image_pack", "im.ponies.room_emotes"];

/// A custom emoji from an image pack.
#[derive(Clone, PartialEq, Eq)]
pub struct CustomEmoji {
    /// Without the colons.
    pub shortcode: String,
    /// A validated `mxc://` address.
    pub url: String,
    pub body: Option<String>,
    pub pack_name: String,
}

/// Hand-written: shortcodes, names and addresses stay out of logs.
impl fmt::Debug for CustomEmoji {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CustomEmoji").finish_non_exhaustive()
    }
}

/// A sticker from an image pack.
#[derive(Clone, PartialEq, Eq)]
pub struct StickerItem {
    pub shortcode: String,
    /// What the sticker is called: its `body`, or else its shortcode.
    pub name: String,
    /// A validated `mxc://` address.
    pub url: String,
    pub pack_name: String,
    pub dimensions: Option<(u32, u32)>,
    pub size: Option<u64>,
    pub mime: Option<String>,
}

/// Hand-written: shortcodes, names and addresses stay out of logs.
impl fmt::Debug for StickerItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StickerItem")
            .field("dimensions", &self.dimensions)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// One image pack: custom emoji, stickers, or both.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct EmotePack {
    pub id: String,
    pub name: String,
    pub emojis: Vec<CustomEmoji>,
    pub stickers: Vec<StickerItem>,
}

/// Hand-written: the pack's id holds a room id.
impl fmt::Debug for EmotePack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EmotePack")
            .field("emojis", &self.emojis.len())
            .field("stickers", &self.stickers.len())
            .finish_non_exhaustive()
    }
}

/// The packs of the account and of the open room, most personal first.
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
    /// Every custom emoji, the first pack winning where shortcodes repeat.
    pub fn emojis(&self) -> impl Iterator<Item = &CustomEmoji> {
        let mut seen = HashSet::new();
        self.packs
            .iter()
            .flat_map(|pack| &pack.emojis)
            .filter(move |emoji| seen.insert(emoji.shortcode.as_str()))
    }

    /// Every sticker, the same picture only once.
    pub fn stickers(&self) -> Vec<&StickerItem> {
        let mut seen = HashSet::new();
        self.packs
            .iter()
            .flat_map(|pack| &pack.stickers)
            .filter(|sticker| seen.insert(sticker.url.as_str()))
            .collect()
    }

    /// The shortcode of the picture at `url` (from an emoji, or else a sticker), for
    /// showing a reaction made with it.
    pub fn shortcode_for_url(&self, url: &str) -> Option<&str> {
        let emoji = self
            .packs
            .iter()
            .flat_map(|pack| &pack.emojis)
            .find(|emoji| emoji.url == url)
            .map(|emoji| emoji.shortcode.as_str());
        emoji.or_else(|| {
            self.packs
                .iter()
                .flat_map(|pack| &pack.stickers)
                .find(|sticker| sticker.url == url)
                .map(|sticker| sticker.shortcode.as_str())
        })
    }
}

/// Whether `url` is an `mxc://` address that can go into a message and into HTML as it
/// is: a valid server name and a media id of letters, digits, `-` and `_` only.
pub fn is_valid_mxc(url: &str) -> bool {
    url.len() <= MAX_MXC_LEN
        && <&MxcUri>::from(url).validate().is_ok()
        && url.rsplit_once('/').is_some_and(|(_, id)| !id.is_empty())
}

fn clean_shortcode(raw: &str) -> Option<String> {
    let code = sanitize_line(raw.trim().trim_matches(':'));
    let ok = !code.is_empty()
        && code.len() <= MAX_SHORTCODE_LEN
        && !is_blank(&code)
        && !code.contains(|c: char| c.is_whitespace() || c == ':');
    ok.then_some(code)
}

/// A name from a pack, safe and short enough to show; `None` when nothing is left.
fn clean_name(raw: &str) -> Option<String> {
    let name = sanitize_line(raw.trim());
    (!is_blank(&name)).then(|| truncate(name.trim(), MAX_NAME_WIDTH))
}

/// A MIME type is only passed on when it looks like one.
fn clean_mime(raw: &str) -> Option<String> {
    let ok = raw.len() <= 64
        && raw
            .split_once('/')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty())
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-/".contains(&b));
    ok.then(|| raw.to_owned())
}

fn parse_usage(arr: Option<&Vec<Value>>, default: (bool, bool)) -> (bool, bool) {
    let Some(items) = arr else {
        return default;
    };
    let emoticon = items.iter().any(|i| i.as_str() == Some("emoticon"));
    let sticker = items.iter().any(|i| i.as_str() == Some("sticker"));
    // No (known) usage means both.
    if emoticon || sticker {
        (emoticon, sticker)
    } else {
        default
    }
}

/// Reads one pack's content (`im.ponies.user_emotes`, `im.ponies.room_emotes` or
/// `m.room.image_pack`). `None` when it holds no usable image.
pub fn parse_pack_json(raw: &Value, default_name: &str, pack_id: &str) -> Option<EmotePack> {
    let pack_obj = raw.get("pack").and_then(Value::as_object);
    let pack_name = pack_obj
        .and_then(|p| {
            p.get("display_name")
                .or_else(|| p.get("name"))
                .and_then(Value::as_str)
        })
        .and_then(clean_name)
        .or_else(|| clean_name(default_name))
        .unwrap_or_else(|| "Pack".to_owned());
    let pack_usage = parse_usage(
        pack_obj
            .and_then(|p| p.get("usage"))
            .and_then(Value::as_array),
        (true, true),
    );

    let mut emojis = Vec::new();
    let mut stickers = Vec::new();
    let mut taken = 0;

    if let Some(images) = raw.get("images").and_then(Value::as_object) {
        for (raw_key, entry) in images {
            if taken >= MAX_IMAGES_PER_PACK {
                break;
            }
            let Some(shortcode) = clean_shortcode(raw_key) else {
                continue;
            };
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let Some(url) = obj
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| is_valid_mxc(u))
            else {
                continue;
            };
            taken += 1;
            let body = obj.get("body").and_then(Value::as_str).and_then(clean_name);
            let (is_emote, is_sticker) =
                parse_usage(obj.get("usage").and_then(Value::as_array), pack_usage);
            let info = obj.get("info").and_then(Value::as_object);
            let dimensions = info.and_then(|inf| {
                let w = u32::try_from(inf.get("w")?.as_u64()?).ok()?;
                let h = u32::try_from(inf.get("h")?.as_u64()?).ok()?;
                (w > 0 && h > 0).then_some((w, h))
            });
            let size = info.and_then(|inf| inf.get("size")?.as_u64());
            let mime = info
                .and_then(|inf| inf.get("mimetype")?.as_str())
                .and_then(clean_mime);

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

    // The oldest `im.ponies.user_emotes` form: `shortcodes: { "name": "mxc://…" }`.
    if emojis.is_empty()
        && stickers.is_empty()
        && let Some(shortcodes) = raw.get("shortcodes").and_then(Value::as_object)
    {
        for (raw_key, val) in shortcodes {
            if emojis.len() >= MAX_IMAGES_PER_PACK {
                break;
            }
            let Some(shortcode) = clean_shortcode(raw_key) else {
                continue;
            };
            let url = val
                .as_str()
                .or_else(|| val.get("url").and_then(Value::as_str));
            let Some(url) = url.filter(|u| is_valid_mxc(u)) else {
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

/// Reads `im.ponies.emote_rooms` or `m.image_pack.rooms` account data: the rooms whose
/// packs the user has switched on everywhere, with their state keys.
pub fn parse_emote_rooms_json(raw: &Value) -> Vec<(String, Vec<String>)> {
    let Some(rooms) = raw.get("rooms").and_then(Value::as_object) else {
        return Vec::new();
    };
    rooms
        .iter()
        .filter(|(room_id, _)| RoomId::parse(room_id.as_str()).is_ok())
        .take(MAX_PACK_ROOMS)
        .map(|(room_id, keys)| {
            let keys = keys
                .as_object()
                .map(|obj| {
                    obj.keys()
                        .filter(|k| k.len() <= 255)
                        .take(MAX_PACKS_PER_ROOM)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            (room_id.clone(), keys)
        })
        .collect()
}

#[derive(Default)]
struct CacheState {
    /// Set once the UI has asked for packs; nothing is loaded before.
    requested: bool,
    active_room: Option<String>,
    /// `Some(None)`: looked, the account has no pack.
    user_pack: Option<Option<EmotePack>>,
    emote_rooms: Option<Vec<(String, Vec<String>)>>,
    /// By room id: the packs found there, with their state keys.
    room_packs: HashMap<String, Vec<(String, EmotePack)>>,
}

fn lock(state: &Mutex<CacheState>) -> MutexGuard<'_, CacheState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Loads the packs when asked to, and again when they change.
pub(super) struct EmoteManager {
    state: Arc<Mutex<CacheState>>,
    poke: Arc<Notify>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl EmoteManager {
    pub fn new(client: Client, events: UnboundedSender<MatrixEvent>) -> Self {
        let state = Arc::new(Mutex::new(CacheState::default()));
        let poke = Arc::new(Notify::new());
        let task_state = state.clone();
        let task_poke = poke.clone();
        let task = tokio::spawn(async move {
            loop {
                task_poke.notified().await;
                let (requested, active_room) = {
                    let guard = lock(&task_state);
                    (guard.requested, guard.active_room.clone())
                };
                if !requested {
                    continue;
                }
                let catalog = load_catalog(&client, &task_state, active_room.as_deref()).await;
                let _ = events.send(MatrixEvent::EmotePacks(catalog));
            }
        });
        Self {
            state,
            poke,
            task: Some(task),
        }
    }

    /// Loads (or sends again) the packs of the account and of `room_id`.
    pub fn ensure(&self, room_id: Option<String>) {
        {
            let mut guard = lock(&self.state);
            guard.requested = true;
            if room_id.is_some() {
                guard.active_room = room_id;
            }
        }
        self.poke.notify_one();
    }

    /// Drops what changed from the cache when pack events arrive, and loads again if the
    /// packs are in use.
    pub fn register_handlers(
        &self,
        client: &Client,
    ) -> [matrix_sdk::event_handler::EventHandlerHandle; 2] {
        use matrix_sdk::ruma::events::{AnyGlobalAccountDataEvent, AnySyncStateEvent};
        use matrix_sdk::ruma::serde::Raw;

        let state = self.state.clone();
        let poke = self.poke.clone();
        let global_handle = client.add_event_handler(move |ev: Raw<AnyGlobalAccountDataEvent>| {
            let state = state.clone();
            let poke = poke.clone();
            async move {
                let Ok(Some(ev_type)) = ev.get_field::<String>("type") else {
                    return;
                };
                let is_user = ev_type == "im.ponies.user_emotes";
                let is_rooms =
                    ev_type == "im.ponies.emote_rooms" || ev_type == "m.image_pack.rooms";
                if !is_user && !is_rooms {
                    return;
                }
                let requested = {
                    let mut guard = lock(&state);
                    if is_user {
                        guard.user_pack = None;
                    } else {
                        guard.emote_rooms = None;
                    }
                    guard.requested
                };
                if requested {
                    poke.notify_one();
                }
            }
        });

        let state = self.state.clone();
        let poke = self.poke.clone();
        let room_handle =
            client.add_event_handler(move |ev: Raw<AnySyncStateEvent>, room: matrix_sdk::Room| {
                let state = state.clone();
                let poke = poke.clone();
                async move {
                    let Ok(Some(ev_type)) = ev.get_field::<String>("type") else {
                        return;
                    };
                    if !ROOM_PACK_TYPES.contains(&ev_type.as_str()) {
                        return;
                    }
                    let requested = {
                        let mut guard = lock(&state);
                        guard.room_packs.remove(room.room_id().as_str());
                        guard.requested
                    };
                    if requested {
                        poke.notify_one();
                    }
                }
            });

        [global_handle, room_handle]
    }

    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// The content of global account data of type `event_type`: from the store, or else
/// from the homeserver.
async fn fetch_global_json(client: &Client, event_type: &str) -> Option<Value> {
    let ty = GlobalAccountDataEventType::from(event_type);
    let raw = match client.account().account_data_raw(ty.clone()).await {
        Ok(Some(raw)) => raw,
        _ => client.account().fetch_account_data(ty).await.ok()??,
    };
    serde_json::from_str(raw.json().get()).ok()
}

/// The packs of one room, with their state keys. What the store has is used first; the
/// sliding sync does not bring these events along, so the usual state keys (the empty
/// one, and those the account data names) are then asked for one by one: the whole
/// state of a big room would be megabytes.
async fn load_room_packs(
    client: &Client,
    room_id: &str,
    listed: &[String],
) -> Vec<(String, EmotePack)> {
    let Ok(room_id) = RoomId::parse(room_id) else {
        return Vec::new();
    };
    let Some(room) = client.get_room(&room_id) else {
        return Vec::new();
    };
    // A room the account is not in (any more) is not asked about.
    if room.state() != matrix_sdk::RoomState::Joined {
        return Vec::new();
    }
    let room_name = room.name().unwrap_or_default();

    let mut found: Vec<(String, Value)> = Vec::new();
    let mut seen = HashSet::new();
    for ty in ROOM_PACK_TYPES {
        let Ok(events) = room.get_state_events(StateEventType::from(ty)).await else {
            continue;
        };
        for raw in events {
            let json = match &raw {
                RawAnySyncOrStrippedState::Sync(raw) => raw.json(),
                RawAnySyncOrStrippedState::Stripped(raw) => raw.json(),
            };
            let Ok(val) = serde_json::from_str::<Value>(json.get()) else {
                continue;
            };
            let key = val.get("state_key").and_then(Value::as_str).unwrap_or("");
            if let Some(content) = val.get("content")
                && found.len() < MAX_PACKS_PER_ROOM
                && seen.insert(key.to_owned())
            {
                found.push((key.to_owned(), content.clone()));
            }
        }
    }

    let mut keys = vec![String::new()];
    for key in listed {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    keys.truncate(MAX_PACKS_PER_ROOM);
    for key in keys {
        for ty in ROOM_PACK_TYPES {
            if seen.contains(&key) || found.len() >= MAX_PACKS_PER_ROOM {
                continue;
            }
            let request = get_state_event_for_key::v3::Request::new(
                room_id.clone(),
                StateEventType::from(ty),
                key.clone(),
            );
            let response = tokio::time::timeout(STATE_FETCH_TIMEOUT, client.send(request)).await;
            if let Ok(Ok(response)) = response
                && let Ok(content) = serde_json::from_str::<Value>(response.event_or_content.get())
            {
                seen.insert(key.clone());
                found.push((key.clone(), content));
            }
        }
    }

    found
        .into_iter()
        .filter_map(|(key, content)| {
            let id = format!("{room_id}|{key}");
            parse_pack_json(&content, &room_name, &id).map(|pack| (key, pack))
        })
        .collect()
}

async fn load_catalog(
    client: &Client,
    state: &Mutex<CacheState>,
    active_room: Option<&str>,
) -> EmoteCatalog {
    if lock(state).user_pack.is_none() {
        let user_pack = fetch_global_json(client, "im.ponies.user_emotes")
            .await
            .and_then(|val| parse_pack_json(&val, "Personal", "user"));
        lock(state).user_pack = Some(user_pack);
    }

    if lock(state).emote_rooms.is_none() {
        let mut rooms = Vec::new();
        if let Some(val) = fetch_global_json(client, "m.image_pack.rooms").await {
            rooms.extend(parse_emote_rooms_json(&val));
        }
        if let Some(val) = fetch_global_json(client, "im.ponies.emote_rooms").await {
            for (rid, keys) in parse_emote_rooms_json(&val) {
                if let Some((_, existing)) = rooms.iter_mut().find(|(r, _)| *r == rid) {
                    for k in keys {
                        if !existing.contains(&k) && existing.len() < MAX_PACKS_PER_ROOM {
                            existing.push(k);
                        }
                    }
                } else {
                    rooms.push((rid, keys));
                }
            }
        }
        rooms.truncate(MAX_PACK_ROOMS);
        lock(state).emote_rooms = Some(rooms);
    }

    let emote_rooms = lock(state).emote_rooms.clone().unwrap_or_default();
    let mut targets: Vec<(String, Vec<String>)> = Vec::new();
    if let Some(rid) = active_room {
        let listed = emote_rooms
            .iter()
            .find(|(r, _)| r == rid)
            .map(|(_, keys)| keys.clone())
            .unwrap_or_default();
        targets.push((rid.to_owned(), listed));
    }
    for (rid, keys) in &emote_rooms {
        if !targets.iter().any(|(r, _)| r == rid) {
            targets.push((rid.clone(), keys.clone()));
        }
    }
    for (rid, listed) in &targets {
        if !lock(state).room_packs.contains_key(rid) {
            let loaded = load_room_packs(client, rid, listed).await;
            lock(state).room_packs.insert(rid.clone(), loaded);
        }
    }

    let guard = lock(state);
    let mut packs = Vec::new();
    let mut seen = HashSet::new();
    if let Some(Some(user_pack)) = &guard.user_pack {
        seen.insert(user_pack.id.clone());
        packs.push(user_pack.clone());
    }
    // The open room's packs all count; elsewhere only those the user switched on.
    if let Some(rid) = active_room
        && let Some(room_packs) = guard.room_packs.get(rid)
    {
        for (_, pack) in room_packs {
            if seen.insert(pack.id.clone()) {
                packs.push(pack.clone());
            }
        }
    }
    for (rid, keys) in &emote_rooms {
        let Some(room_packs) = guard.room_packs.get(rid) else {
            continue;
        };
        for (key, pack) in room_packs {
            if (keys.is_empty() || keys.contains(key)) && seen.insert(pack.id.clone()) {
                packs.push(pack.clone());
            }
        }
    }
    packs.truncate(MAX_PACKS);
    EmoteCatalog { packs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn packs_are_read_with_their_usage_and_debug_shows_nothing_private() {
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
        assert_eq!(pack.stickers[1].name, "Waving Ferris");
        assert_eq!(pack.stickers[1].dimensions, Some((256, 256)));
        assert_eq!(pack.stickers[1].size, Some(12345));
        assert_eq!(pack.stickers[1].mime.as_deref(), Some("image/webp"));

        let catalog = EmoteCatalog { packs: vec![pack] };
        assert_eq!(
            catalog.shortcode_for_url("mxc://example.org/crab1"),
            Some("crab_happy")
        );
        // A picture that is only a sticker still has a name for reactions.
        assert_eq!(
            catalog.shortcode_for_url("mxc://example.org/crab2"),
            Some("crab_wave")
        );
        assert_eq!(catalog.shortcode_for_url("mxc://example.org/nope"), None);

        let dbg = format!(
            "{catalog:?} {:?} {:?} {:?}",
            catalog.packs[0], catalog.packs[0].emojis[0], catalog.packs[0].stickers[0]
        );
        assert!(!dbg.contains("Ferris"), "debug leaked pack name: {dbg}");
        assert!(
            !dbg.contains("crab"),
            "debug leaked a shortcode or address: {dbg}"
        );
        assert!(!dbg.contains("room1"), "debug leaked the pack id: {dbg}");
    }

    #[test]
    fn pack_rooms_and_the_oldest_shortcode_form_are_read() {
        let rooms_json = json!({
            "rooms": {
                "!pack1:example.org": { "ponies": {}, "": {} },
                "not a room": { "": {} }
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
        assert_eq!(pack.name, "Personal");
        assert_eq!(pack.emojis.len(), 1);
        assert_eq!(pack.emojis[0].shortcode, "party_blob");
        assert!(pack.stickers.is_empty());
    }

    #[test]
    fn hostile_packs_are_cut_down_and_cleaned() {
        let mut images = serde_json::Map::new();
        for n in 0..MAX_IMAGES_PER_PACK + 50 {
            images.insert(
                format!("e{n:04}"),
                json!({ "url": format!("mxc://example.org/m{n}") }),
            );
        }
        let long = "x".repeat(MAX_SHORTCODE_LEN + 1);
        for (code, url) in [
            (long.as_str(), "mxc://example.org/long"),
            ("two words", "mxc://example.org/space"),
            (":colon:in:", "mxc://example.org/colon"),
            ("\u{200b}", "mxc://example.org/blank"),
            ("aaa_quote", "mxc://example.org/a\"b"),
            ("aaa_tag", "mxc://example.org/<b>"),
            ("aaa_noid", "mxc://example.org/"),
            ("aaa_noserver", "mxc:///media"),
        ] {
            images.insert(code.to_owned(), json!({ "url": url }));
        }
        images.insert(
            "aaa_ok".to_owned(),
            json!({
                "url": "mxc://example.org/ok",
                "body": "Line\none\u{1b}[31m",
                "info": { "mimetype": "image/png\"><script>" }
            }),
        );
        let raw = json!({
            "pack": { "display_name": format!("Evil\u{7}{}", "n".repeat(200)) },
            "images": images
        });
        let pack = parse_pack_json(&raw, "Fallback", "p").expect("pack");
        assert_eq!(pack.emojis.len(), MAX_IMAGES_PER_PACK);
        assert!(pack.name.starts_with("Eviln"), "{}", pack.name);
        assert!(crate::text::width(&pack.name) <= MAX_NAME_WIDTH);
        let ok = pack
            .stickers
            .iter()
            .find(|s| s.shortcode == "aaa_ok")
            .expect("the good one is kept");
        assert_eq!(ok.name, "Line one[31m");
        assert_eq!(ok.mime, None);
        for emoji in &pack.emojis {
            assert!(is_valid_mxc(&emoji.url), "{}", emoji.url);
            assert!(emoji.shortcode.len() <= MAX_SHORTCODE_LEN);
            assert!(!emoji.shortcode.contains([' ', ':']));
        }
    }

    #[test]
    fn only_plain_mxc_addresses_are_valid() {
        assert!(is_valid_mxc("mxc://example.org/abc-DEF_123"));
        assert!(is_valid_mxc("mxc://[::1]:8448/abc"));
        for bad in [
            "https://example.org/abc",
            "mxc://example.org/",
            "mxc://example.org/a/b",
            "mxc://exa mple.org/abc",
            "mxc://example.org/a\"onerror=\"x",
            "mxc://example.org/abc?x=1",
            "mxc://",
        ] {
            assert!(!is_valid_mxc(bad), "{bad}");
        }
        assert!(!is_valid_mxc(&format!(
            "mxc://example.org/{}",
            "a".repeat(MAX_MXC_LEN)
        )));
    }

    #[test]
    fn the_catalog_lists_each_shortcode_and_each_sticker_once() {
        let pack = |id: &str, url: &str| EmotePack {
            id: id.into(),
            name: id.into(),
            emojis: vec![CustomEmoji {
                shortcode: "blob".into(),
                url: url.into(),
                body: None,
                pack_name: id.into(),
            }],
            stickers: vec![StickerItem {
                shortcode: "blob".into(),
                name: "Blob".into(),
                url: url.into(),
                pack_name: id.into(),
                dimensions: None,
                size: None,
                mime: None,
            }],
        };
        let catalog = EmoteCatalog {
            packs: vec![
                pack("mine", "mxc://example.org/a"),
                pack("room", "mxc://example.org/b"),
                pack("again", "mxc://example.org/a"),
            ],
        };
        let emojis: Vec<_> = catalog.emojis().map(|e| e.url.as_str()).collect();
        assert_eq!(emojis, ["mxc://example.org/a"]);
        let stickers: Vec<_> = catalog
            .stickers()
            .iter()
            .map(|s| s.pack_name.as_str())
            .collect();
        assert_eq!(stickers, ["mine", "room"]);
    }
}
