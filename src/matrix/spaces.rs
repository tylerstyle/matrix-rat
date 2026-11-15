//! Spaces: which of the user's rooms belong to which space, for filtering the room
//! list; the rooms of a space for its info; and putting rooms into spaces and taking
//! them out. The SDK keeps the hierarchy up to date; this turns it into plain data.

use std::collections::HashMap;
use std::time::Duration;

use matrix_sdk::deserialized_responses::RawSyncOrStrippedState;
use matrix_sdk::ruma::api::client::room::create_room::v3::{
    CreationContent, Request as CreateRequest, RoomPreset,
};
use matrix_sdk::ruma::api::client::space::get_hierarchy;
use matrix_sdk::ruma::events::StateEventType;
use matrix_sdk::ruma::events::space::child::SpaceChildEventContent;
use matrix_sdk::ruma::events::space::parent::SpaceParentEventContent;
use matrix_sdk::ruma::room::RoomType;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{
    MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedServerName, OwnedSpaceChildOrder, RoomId, UInt,
    UserId,
};
use matrix_sdk::{Client, Room, RoomState};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::spaces::SpaceFilter;
use tokio::sync::mpsc::UnboundedSender;

use super::MatrixEvent;
use super::left::LeftRooms;
use super::manage::{ManageEvent, failed};
use crate::text::{sanitize, sanitize_line, truncate};

/// The most rooms of one space that are listed or worked with: a space can name any
/// number of them.
pub const CHILDREN_MAX: usize = 500;
/// The most rooms of one space read before the rest are left alone.
const EVENTS_MAX: usize = 5_000;
/// The widest name kept for a room of a space.
const NAME_MAX: usize = 255;
/// How long the server gets, all pages together, to name the rooms of a space that
/// we are not in: meanwhile, the open info of the space shows nothing new.
const HIERARCHY_TIMEOUT: Duration = Duration::from_secs(10);
/// Pages of that list asked for at most, and rooms on each: as many as are listed.
const HIERARCHY_PAGES: usize = 5;
const HIERARCHY_PAGE: u32 = (CHILDREN_MAX / HIERARCHY_PAGES) as u32;

/// A space the room list can be narrowed to.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SpaceSummary {
    pub id: String,
    pub name: String,
    /// 0 for a top-level space, 1 for a space inside one.
    pub level: u8,
    /// Ids of the rooms in it (for a top-level space: directly in it).
    pub rooms: Vec<String>,
    /// Ids of what is directly in it (rooms and spaces), at most `CHILDREN_MAX`.
    pub children: Vec<String>,
    /// We may put rooms into it and take them out.
    pub can_edit: bool,
}

/// Hand-written: which space, its name and its rooms stay out of logs.
impl std::fmt::Debug for SpaceSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SpaceSummary(level {}, {} rooms, {} children, can_edit: {})",
            self.level,
            self.rooms.len(),
            self.children.len(),
            self.can_edit
        )
    }
}

/// The spaces for the room list, with what is directly in each and whether we may
/// change that (both read from the store: the SDK's filters have neither).
pub async fn summarize(
    client: &Client,
    filters: &Vector<SpaceFilter>,
    left: &LeftRooms,
) -> Vec<SpaceSummary> {
    let mut out = Vec::with_capacity(filters.len());
    for filter in filters {
        let Some(mut summary) = summary_of(filter, left) else {
            continue;
        };
        if let Some(space) = client.get_room(&filter.space_room.room_id) {
            summary.children = child_events(&space)
                .await
                .0
                .into_iter()
                .filter(|child| !left.is_left(&child.id))
                .map(|child| child.id.to_string())
                .collect();
            summary.can_edit = may_edit(&space, left).await;
        }
        out.push(summary);
    }
    out
}

/// `None` for a space that was left here: it (and a room left here inside a space) can
/// look joined to the SDK (see `super::left`), and is not one of the user's any more.
fn summary_of(filter: &SpaceFilter, left: &LeftRooms) -> Option<SpaceSummary> {
    if left.is_left(&filter.space_room.room_id) {
        return None;
    }
    Some(SpaceSummary {
        id: filter.space_room.room_id.to_string(),
        name: space_name(&filter.space_room.display_name),
        level: filter.level,
        rooms: filter
            .descendants
            .iter()
            .filter(|id| !left.is_left(id))
            .map(|id| id.to_string())
            .collect(),
        ..SpaceSummary::default()
    })
}

// ----- the rooms of a space ----------------------------------------------------------

/// What the info of a space shows about the rooms in it. All text is safe to show.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SpaceDetails {
    /// In the order the space gives them.
    pub children: Vec<SpaceChild>,
    /// The space names more rooms than are listed.
    pub more: bool,
    /// We may put rooms into it, take them out and suggest them.
    pub can_edit: bool,
}

/// Counts only: which rooms are in a space is nobody's business in a log.
impl std::fmt::Debug for SpaceDetails {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SpaceDetails({} children, more: {}, can_edit: {})",
            self.children.len(),
            self.more,
            self.can_edit
        )
    }
}

/// A room (or space) in a space.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SpaceChild {
    pub room_id: String,
    /// Empty when neither this client nor the server could say.
    pub name: String,
    /// We are in it, as far as the worker knows.
    pub joined: bool,
    /// It is a space itself.
    pub space: bool,
    /// The space's admins point it out.
    pub suggested: bool,
    /// Servers to join it through, as the space names them.
    pub via: Vec<String>,
}

impl std::fmt::Debug for SpaceChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SpaceChild(joined: {}, space: {}, suggested: {}, via {} servers)",
            self.joined,
            self.space,
            self.suggested,
            self.via.len()
        )
    }
}

/// A room of a space, as its `m.space.child` event has it.
struct ChildEvent {
    id: OwnedRoomId,
    via: Vec<OwnedServerName>,
    order: Option<OwnedSpaceChildOrder>,
    suggested: bool,
    sent: MilliSecondsSinceUnixEpoch,
}

/// What is directly in `space`, in the order the specification asks for, at most
/// `CHILDREN_MAX`; `true` if there is more.
async fn child_events(space: &Room) -> (Vec<ChildEvent>, bool) {
    let Ok(events) = space
        .get_state_events_static::<SpaceChildEventContent>()
        .await
    else {
        return (Vec::new(), false);
    };
    order_children(live_children(&events))
}

/// The rooms that `events` put into a space, at most `EVENTS_MAX`. A room that was
/// taken out has empty content, which does not read as a child; neither does
/// anything malformed. Invitations show no rooms. Only rooms that are in the space
/// count towards the bound: many that came and went do not hide the ones there.
fn live_children(events: &[RawSyncOrStrippedState<SpaceChildEventContent>]) -> Vec<ChildEvent> {
    events
        .iter()
        .filter_map(|raw| {
            let RawSyncOrStrippedState::Sync(raw) = raw else {
                return None;
            };
            let event = raw.deserialize().ok()?;
            let event = event.as_original()?;
            Some(ChildEvent {
                id: event.state_key.clone(),
                via: event.content.via.clone(),
                order: event.content.order.clone(),
                suggested: event.content.suggested,
                sent: event.origin_server_ts,
            })
        })
        .take(EVENTS_MAX)
        .collect()
}

/// By `order` where the space gives one, then the oldest first, then by id; cut at
/// `CHILDREN_MAX`.
fn order_children(mut children: Vec<ChildEvent>) -> (Vec<ChildEvent>, bool) {
    use std::cmp::Ordering;
    children.sort_by(|a, b| match (&a.order, &b.order) {
        (Some(x), Some(y)) => x.cmp(y).then(a.sent.cmp(&b.sent)).then(a.id.cmp(&b.id)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.sent.cmp(&b.sent).then(a.id.cmp(&b.id)),
    });
    let more = children.len() > CHILDREN_MAX;
    children.truncate(CHILDREN_MAX);
    (children, more)
}

/// Whether we may change what is in `space`. Without power levels in the store, the
/// rules of a room that has none apply (its creators may), as for the room info. Not
/// in a space left here, which can look joined while it is not (see `super::left`).
async fn may_edit(space: &Room, left: &LeftRooms) -> bool {
    space.state() == RoomState::Joined
        && !left.gone(space)
        && space
            .power_levels_or_default()
            .await
            .user_can_send_state(space.own_user_id(), StateEventType::SpaceChild)
}

/// What the server says about the rooms of a space: name, and whether it is a space,
/// by room id.
pub(super) type Named = HashMap<String, (String, bool)>;

fn clean_name(name: &str) -> String {
    truncate(sanitize_line(name).trim(), NAME_MAX)
}

/// The name of a space for the room list, as any other name kept here: safe, not
/// too wide, and never blank.
fn space_name(display_name: &str) -> String {
    let name = clean_name(display_name);
    if crate::text::is_blank(&name) {
        "Unnamed space".to_owned()
    } else {
        name
    }
}

/// The rooms of `space` for its info. Names come from this client where it knows the
/// room, else from `named`.
pub(super) async fn space_details(
    client: &Client,
    space: &Room,
    named: &Named,
    left: &LeftRooms,
) -> SpaceDetails {
    let (events, more) = child_events(space).await;
    let mut children = Vec::with_capacity(events.len());
    for event in events {
        let told = named.get(event.id.as_str());
        let (mut name, joined, is_space) = match client.get_room(&event.id) {
            // A room that was left long ago is only an id here; the server knows more.
            Some(room) if room.state() != RoomState::Left => {
                let name = match room.cached_display_name() {
                    Some(name) => name.to_string(),
                    None => room
                        .display_name()
                        .await
                        .map(|name| name.to_string())
                        .unwrap_or_default(),
                };
                // Not one left here, which can look joined while it is not (see
                // `super::left`): it is offered the way in, as any room we are not
                // in. Its name stays the one known here, from when it was ours.
                (
                    clean_name(&name),
                    room.state() == RoomState::Joined && !left.gone(&room),
                    room.is_space(),
                )
            }
            _ => (String::new(), false, told.is_some_and(|(_, space)| *space)),
        };
        if name.is_empty()
            && let Some((told, _)) = told
        {
            name.clone_from(told);
        }
        children.push(SpaceChild {
            // As it is: it is sent back as a state key. What shows it makes it safe.
            room_id: event.id.to_string(),
            name,
            joined,
            space: is_space,
            suggested: event.suggested,
            via: event
                .via
                .iter()
                .take(crate::manage::VIA_MAX)
                .map(ToString::to_string)
                .collect(),
        });
    }
    SpaceDetails {
        children,
        more,
        can_edit: may_edit(space, left).await,
    }
}

/// Whether some room of the space still has no name, so the server is worth asking.
pub(super) fn names_missing(details: &SpaceDetails) -> bool {
    details.children.iter().any(|child| child.name.is_empty())
}

/// Asks the server for the names of the rooms in `space` (the ones we are not in are
/// not known here). What could not be had is left out; the list is shown anyway.
pub(super) async fn hierarchy_names(client: &Client, space: &Room) -> Named {
    names_within(client, space.room_id(), HIERARCHY_TIMEOUT).await
}

/// `hierarchy_names` of the space `space_id`, given `within` for all of it.
async fn names_within(client: &Client, space_id: &RoomId, within: Duration) -> Named {
    let deadline = tokio::time::Instant::now() + within;
    let mut named = Named::new();
    let mut from = None;
    for _ in 0..HIERARCHY_PAGES {
        let mut request = get_hierarchy::v1::Request::new(space_id.to_owned());
        request.max_depth = Some(UInt::from(1u32));
        request.limit = Some(UInt::from(HIERARCHY_PAGE));
        request.from = from.take();
        let response = match tokio::time::timeout_at(deadline, client.send(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                tracing::warn!(why = %crate::logsafe::kind(&err), "could not list the rooms of a space");
                break;
            }
            Err(_) => {
                tracing::warn!(why = %crate::logsafe::TIMED_OUT, "could not list the rooms of a space");
                break;
            }
        };
        for chunk in response.rooms {
            let summary = chunk.summary;
            // The space itself comes first: it is not one of its rooms.
            if summary.room_id.as_str() == space_id.as_str() {
                continue;
            }
            if named.len() >= CHILDREN_MAX {
                break;
            }
            let name = summary
                .name
                .filter(|name| !crate::text::is_blank(name))
                .or_else(|| summary.canonical_alias.map(|alias| alias.to_string()))
                .unwrap_or_default();
            named.insert(
                summary.room_id.to_string(),
                (
                    clean_name(&name),
                    summary.room_type == Some(RoomType::Space),
                ),
            );
        }
        match response.next_batch {
            Some(next) if named.len() < CHILDREN_MAX => from = Some(next),
            _ => break,
        }
    }
    named
}

// ----- changing a space --------------------------------------------------------------

/// What can be changed about a room in a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceEdit {
    Add,
    Remove,
    /// Point it out to the members of the space, or no longer.
    Suggest(bool),
}

impl SpaceEdit {
    /// For "Could not …".
    fn what(self) -> &'static str {
        match self {
            Self::Add => "add the room to the space",
            Self::Remove => "remove the room from the space",
            Self::Suggest(true) => "suggest the room",
            Self::Suggest(false) => "stop suggesting the room",
        }
    }
}

/// Creates the space and tells who asked which it is, or why there is none.
pub(super) async fn answer_create(
    client: Client,
    events: UnboundedSender<MatrixEvent>,
    request: u64,
    name: String,
    topic: Option<String>,
    alias: Option<String>,
) {
    let result = create_space(&client, name, topic, alias).await;
    let _ = events.send(MatrixEvent::Manage(ManageEvent::SpaceCreated {
        request,
        result,
    }));
}

/// Changes the space and tells who asked how it went. The open info of the space is
/// not asked to look again here: the store has the change only with the next sync,
/// whose `m.space.child` event does that.
pub(super) async fn answer_edit(
    client: Client,
    left: LeftRooms,
    events: UnboundedSender<MatrixEvent>,
    request: u64,
    space_id: String,
    child_id: String,
    edit: SpaceEdit,
) {
    let result = edit_space(&client, &left, &space_id, &child_id, edit).await;
    let _ = events.send(MatrixEvent::Manage(ManageEvent::SpaceEdited {
        request,
        space_id,
        child_id,
        edit,
        result,
    }));
}

/// A space: public with an address, or by invitation. Its id, or what went wrong.
async fn create_space(
    client: &Client,
    name: String,
    topic: Option<String>,
    alias: Option<String>,
) -> Result<String, String> {
    let mut request = CreateRequest::new();
    request.name = Some(sanitize_line(&name));
    request.topic = topic.map(|topic| sanitize(&topic));
    let mut content = CreationContent::new();
    content.room_type = Some(RoomType::Space);
    request.creation_content = Some(
        Raw::new(&content).map_err(|_| "Could not create the space: it could not be described.")?,
    );
    match alias {
        // Anyone may join by the address; publishing it in the server's directory is
        // a decision of its own, as for rooms.
        Some(alias) => {
            request.preset = Some(RoomPreset::PublicChat);
            request.room_alias_name = Some(alias);
        }
        None => request.preset = Some(RoomPreset::PrivateChat),
    }
    let created = client
        .create_room(request)
        .await
        .map_err(|err| failed("create the space", &err))?;
    Ok(created.room_id().to_string())
}

/// A space or room left here can look joined while it is not (see `super::left`):
/// `left` tells those apart.
async fn edit_space(
    client: &Client,
    left: &LeftRooms,
    space_id: &str,
    child_id: &str,
    edit: SpaceEdit,
) -> Result<(), String> {
    let what = edit.what();
    let space = super::manage::room(client, space_id)?;
    let child =
        RoomId::parse(child_id).map_err(|_| format!("Could not {what}: that is not a room id."))?;
    if space.state() != RoomState::Joined || left.gone(&space) || !space.is_space() {
        return Err(format!("Could not {what}: you are not in that space."));
    }
    if !may_edit(&space, left).await {
        return Err(format!(
            "Could not {what}: you are not allowed to change the rooms of this space."
        ));
    }
    let own = space.own_user_id().to_owned();
    match edit {
        SpaceEdit::Add => {
            if child == space.room_id() {
                return Err("A space cannot be put into itself.".to_owned());
            }
            let room = client
                .get_room(&child)
                .filter(|room| room.state() == RoomState::Joined && !left.gone(room))
                .ok_or_else(|| format!("Could not {what}: you are not in that room."))?;
            // Sent again, its order and suggestion would be lost.
            if child_content(&space, &child).await.is_some() {
                return Err(format!("Could not {what}: it is in this space already."));
            }
            let content = SpaceChildEventContent::new(route(&room, &own).await);
            space
                .send_state_event_for_key(&child, content)
                .await
                .map_err(|err| failed(what, &err))?;
            link_parent(&room, &space, &own, true).await;
        }
        SpaceEdit::Remove => {
            // Empty content, not a redaction: a redacted state event takes no part in
            // state resolution the way an emptied one does.
            space
                .send_state_event_raw("m.space.child", child.as_str(), serde_json::json!({}))
                .await
                .map_err(|err| failed(what, &err))?;
            // Only in a room we are in: the server would refuse it anyway.
            if let Some(room) = client.get_room(&child).filter(|room| !left.gone(room)) {
                link_parent(&room, &space, &own, false).await;
            }
        }
        SpaceEdit::Suggest(on) => {
            let Some(mut content) = child_content(&space, &child).await else {
                return Err(format!("Could not {what}: it is no longer in this space."));
            };
            // Servers and order stay as they are.
            content.suggested = on;
            space
                .send_state_event_for_key(&child, content)
                .await
                .map_err(|err| failed(what, &err))?;
        }
    }
    Ok(())
}

/// The `m.space.child` content naming `child` in `space`, if it is in it.
async fn child_content(space: &Room, child: &RoomId) -> Option<SpaceChildEventContent> {
    let raw = space
        .get_state_event_static_for_key::<SpaceChildEventContent, _>(child)
        .await
        .ok()
        .flatten()?;
    let RawSyncOrStrippedState::Sync(raw) = raw else {
        return None;
    };
    let event = raw.deserialize().ok()?;
    event.as_original().map(|event| event.content.clone())
}

/// The room's `m.space.parent` content naming `space`, if it links to it: one that
/// was emptied or redacted does not.
async fn parent_content(room: &Room, space: &RoomId) -> Option<SpaceParentEventContent> {
    let raw = room
        .get_state_event_static_for_key::<SpaceParentEventContent, _>(space)
        .await
        .ok()
        .flatten()?;
    let RawSyncOrStrippedState::Sync(raw) = raw else {
        return None;
    };
    let event = raw.deserialize().ok()?;
    event.as_original().map(|event| event.content.clone())
}

/// The room's own half of the link (`m.space.parent`), where we may set it. The
/// space alone decides what is in it, so this is a courtesy: failing here is logged,
/// not reported.
async fn link_parent(room: &Room, space: &Room, own: &UserId, add: bool) {
    if room.state() != RoomState::Joined {
        return;
    }
    let allowed = room
        .power_levels_or_default()
        .await
        .user_can_send_state(own, StateEventType::SpaceParent);
    if !allowed {
        return;
    }
    let linked = parent_content(room, space.room_id()).await;
    let result = if add {
        let mut content = SpaceParentEventContent::new(route(space, own).await);
        // Whether this is the room's main space is the room's admins' choice: a
        // link that says so keeps saying it.
        content.canonical = linked.is_some_and(|link| link.canonical);
        room.send_state_event_for_key(space.room_id(), content)
            .await
            .map(|_| ())
    } else {
        // Only a link that is there is taken back.
        if linked.is_none() {
            return;
        }
        room.send_state_event_raw(
            "m.space.parent",
            space.room_id().as_str(),
            serde_json::json!({}),
        )
        .await
        .map(|_| ())
    };
    if let Err(err) = result {
        tracing::warn!(why = %crate::logsafe::kind(&err), "could not update the room's link to its space");
    }
}

/// Servers to reach `room` through: the SDK's choice (where its most powerful and
/// most numerous members are), else our own.
async fn route(room: &Room, own: &UserId) -> Vec<OwnedServerName> {
    let mut via = room.route().await.unwrap_or_default();
    if via.is_empty() {
        via.push(own.server_name().to_owned());
    }
    via.truncate(crate::manage::VIA_MAX);
    via
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use matrix_sdk::Client;
    use matrix_sdk::config::SyncSettings;
    use matrix_sdk::ruma::api::client::room::create_room::v3::{CreationContent, Request};
    use matrix_sdk::ruma::events::space::child::SpaceChildEventContent;
    use matrix_sdk::ruma::room::RoomType;
    use matrix_sdk::ruma::serde::Raw;
    use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, SpaceChildOrder, UInt};
    use serde_json::json;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::matrix::{ManageCmd, MatrixCmd, MatrixEvent, RoomDetails, spawn};
    use crate::paths::Paths;

    #[test]
    fn a_space_or_room_left_here_is_not_offered() {
        use matrix_sdk::ruma::{OwnedRoomId, room_id};
        use matrix_sdk_ui::spaces::{SpaceFilter, SpaceRoom};

        use super::summary_of;
        use crate::matrix::left::LeftRooms;

        let filter = |id: &str, rooms: &[&str]| SpaceFilter {
            space_room: SpaceRoom {
                room_id: OwnedRoomId::try_from(id).unwrap(),
                canonical_alias: None,
                name: None,
                display_name: "A space".to_owned(),
                topic: None,
                avatar_url: None,
                room_type: None,
                num_joined_members: 1,
                join_rule: None,
                world_readable: None,
                guest_can_join: false,
                is_direct: None,
                children_count: 0,
                state: Some(matrix_sdk::RoomState::Joined),
                heroes: None,
                via: Vec::new(),
                suggested: false,
                is_dm: None,
            },
            level: 0,
            descendants: rooms
                .iter()
                .map(|room| OwnedRoomId::try_from(*room).unwrap())
                .collect(),
        };
        let filters: matrix_sdk_ui::eyeball_im::Vector<SpaceFilter> = [
            filter("!kept:localhost", &["!in:localhost", "!out:localhost"]),
            filter("!left:localhost", &["!in:localhost"]),
        ]
        .into_iter()
        .collect();
        let left = LeftRooms::default();
        let summarize = |left: &LeftRooms| -> Vec<SpaceSummary> {
            filters
                .iter()
                .filter_map(|filter| summary_of(filter, left))
                .collect()
        };
        assert_eq!(summarize(&left).len(), 2);
        left.mark(room_id!("!left:localhost"));
        left.mark(room_id!("!out:localhost"));
        let spaces = summarize(&left);
        assert_eq!(spaces.len(), 1);
        assert_eq!(spaces[0].id, "!kept:localhost");
        assert_eq!(spaces[0].rooms, ["!in:localhost"]);
    }

    fn child(id: &str, order: Option<&str>, sent: u32) -> ChildEvent {
        ChildEvent {
            id: OwnedRoomId::try_from(id).unwrap(),
            via: Vec::new(),
            order: order.map(|order| SpaceChildOrder::parse(order).unwrap()),
            suggested: false,
            sent: MilliSecondsSinceUnixEpoch(UInt::from(sent)),
        }
    }

    #[test]
    fn rooms_of_a_space_come_in_its_order_and_are_cut_short() {
        let (ordered, more) = order_children(vec![
            child("!late:example.org", None, 30),
            child("!b:example.org", Some("b"), 50),
            child("!early:example.org", None, 10),
            child("!a:example.org", Some("a"), 90),
        ]);
        let ids: Vec<&str> = ordered.iter().map(|child| child.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "!a:example.org",
                "!b:example.org",
                "!early:example.org",
                "!late:example.org"
            ]
        );
        assert!(!more);
        let many = (0..CHILDREN_MAX + 3)
            .map(|n| child(&format!("!r{n}:example.org"), None, n as u32))
            .collect();
        let (ordered, more) = order_children(many);
        assert_eq!(ordered.len(), CHILDREN_MAX);
        assert!(more);
    }

    #[test]
    fn what_is_in_a_space_stays_out_of_logs() {
        let summary = SpaceSummary {
            id: "!secret:example.org".into(),
            name: "Secret club".into(),
            level: 0,
            rooms: vec!["!hidden:example.org".into()],
            children: vec!["!hidden:example.org".into()],
            can_edit: true,
        };
        let details = SpaceDetails {
            children: vec![SpaceChild {
                room_id: "!hidden:example.org".into(),
                name: "Hidden room".into(),
                via: vec!["secret.example.org".into()],
                ..SpaceChild::default()
            }],
            more: false,
            can_edit: true,
        };
        for text in [
            format!("{summary:?}"),
            format!("{details:?}"),
            format!("{:?}", details.children[0]),
        ] {
            for secret in ["secret", "Secret", "hidden", "Hidden"] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
        assert_eq!(SpaceEdit::Suggest(true).what(), "suggest the room");
    }

    #[test]
    fn space_commands_and_answers_name_nothing_in_logs() {
        let commands = [
            ManageCmd::CreateSpace {
                request: 1,
                name: "Secret club".into(),
                topic: Some("the heist".into()),
                alias: Some("secret-club".into()),
            },
            ManageCmd::EditSpace {
                request: 2,
                space_id: "!secret:example.org".into(),
                child_id: "!hidden:example.org".into(),
                edit: SpaceEdit::Suggest(true),
            },
        ];
        let events = [
            ManageEvent::SpaceCreated {
                request: 1,
                result: Ok("!secret:example.org".into()),
            },
            ManageEvent::SpaceEdited {
                request: 2,
                space_id: "!secret:example.org".into(),
                child_id: "!hidden:example.org".into(),
                edit: SpaceEdit::Remove,
                result: Err("Could not remove !hidden:example.org".into()),
            },
            ManageEvent::Gone {
                room_id: "!secret:example.org".into(),
            },
        ];
        let text = format!("{commands:?} {events:?}");
        for secret in ["ecret", "heist", "hidden"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert_eq!(
            format!("{:?}", commands[1]),
            "EditSpace(request 2, Suggest(true))"
        );
        // Tests may still tell them apart.
        assert_eq!(
            commands[1].describe(),
            "EditSpace(request 2, !secret:example.org, !hidden:example.org, Suggest(true))"
        );
        assert_eq!(
            commands[0].describe(),
            "CreateSpace(request 1, public: true)"
        );
    }

    fn get(client: &Client, id: &str) -> Room {
        client.get_room(&RoomId::parse(id).unwrap()).unwrap()
    }

    #[test]
    fn a_space_is_named_safely_and_not_too_widely() {
        assert_eq!(space_name("  Rat\u{1b}[2J club "), "Rat[2J club");
        assert_eq!(space_name(" \u{200b} "), "Unnamed space");
        let long = space_name(&"x".repeat(NAME_MAX + 50));
        assert_eq!(crate::text::width(&long), NAME_MAX);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn rooms_taken_out_of_a_space_do_not_count_towards_the_bound() {
        let event = |n: usize, content: serde_json::Value| {
            let event = json!({
                "type": "m.space.child",
                "state_key": format!("!r{n}:example.org"),
                "content": content,
                "sender": "@me:example.org",
                "event_id": format!("$e{n}"),
                "origin_server_ts": n,
            });
            RawSyncOrStrippedState::Sync(Raw::from_json_string(event.to_string()).unwrap())
        };
        let mut events: Vec<_> = (0..EVENTS_MAX).map(|n| event(n, json!({}))).collect();
        events.push(event(EVENTS_MAX, json!({ "via": ["example.org"] })));
        let children = live_children(&events);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, format!("!r{EVENTS_MAX}:example.org"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_server_names_the_rooms_of_a_space_page_by_page_but_not_the_space_or_for_long() {
        const SPACE: &str = "!space:localhost";
        let chunk = |id: &str, name: &str| {
            json!({
                "room_id": id,
                "name": name,
                "num_joined_members": 1,
                "world_readable": false,
                "guest_can_join": false,
                "join_rule": "public",
                "children_state": [],
            })
        };
        let first = json!({
            "rooms": [chunk(SPACE, "The space"), chunk("!a:localhost", "Alpha")],
            "next_batch": "more",
        })
        .to_string();
        // The second page never comes.
        let server = fake::Server::answering(
            json!({ "join": { SPACE: fake::joined(true, vec![]) } }),
            Box::new(move |request| {
                if !request.path.ends_with("/hierarchy") {
                    None
                } else if request.query.contains("from=") {
                    Some(fake::Reply::Never)
                } else {
                    Some(fake::Reply::Json(first.clone()))
                }
            }),
        )
        .await;
        let client = fake::client(&server).await;
        let started = std::time::Instant::now();
        let named = names_within(
            &client,
            &RoomId::parse(SPACE).unwrap(),
            Duration::from_millis(500),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(named.len(), 1, "{:?}", named.keys());
        assert_eq!(named["!a:localhost"], ("Alpha".to_owned(), false));
        let asked: Vec<String> = server
            .requests()
            .into_iter()
            .filter(|request| request.path.ends_with("/hierarchy"))
            .map(|request| request.query)
            .collect();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(
            asked[0].contains(&format!("limit={HIERARCHY_PAGE}")),
            "{asked:?}"
        );
        assert!(asked[0].contains("max_depth=1"), "{asked:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn who_made_a_space_may_change_it_while_its_power_levels_are_not_known_here() {
        let server = fake::Server::start(json!({ "join": {
                "!made:localhost": fake::joined(true, vec![]),
                "!ruled:localhost": fake::joined(true, vec![fake::state(
                    "m.room.power_levels",
                    "",
                    json!({ "users": { "@bob:localhost": 100 }, "state_default": 50 }),
                )]),
            } }))
        .await;
        let client = fake::client(&server).await;
        // Power levels that leave us out are kept to.
        let left = LeftRooms::default();
        assert!(!may_edit(&get(&client, "!ruled:localhost"), &left).await);
        assert!(may_edit(&get(&client, "!made:localhost"), &left).await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_room_or_space_left_here_is_shown_as_one_we_are_not_in() {
        const SPACE: &str = "!space:localhost";
        let child = |id: &str| fake::state("m.space.child", id, json!({ "via": ["localhost"] }));
        let server = fake::Server::start(json!({ "join": {
                SPACE: fake::joined(true, vec![child("!room:localhost"), child("!sub:localhost")]),
                "!room:localhost": fake::joined(false, vec![]),
                "!sub:localhost": fake::joined(true, vec![]),
            } }))
        .await;
        let client = fake::client(&server).await;
        let space = get(&client, SPACE);
        let left = LeftRooms::default();
        let rows = |details: &SpaceDetails| -> Vec<(String, bool, bool)> {
            details
                .children
                .iter()
                .map(|child| (child.room_id.clone(), child.joined, child.space))
                .collect()
        };
        let details = space_details(&client, &space, &Named::new(), &left).await;
        assert_eq!(
            rows(&details),
            [
                ("!room:localhost".to_owned(), true, false),
                ("!sub:localhost".to_owned(), true, true)
            ]
        );
        assert!(details.can_edit);
        // Left here, both still look joined: they are offered the way back in.
        for id in ["!room:localhost", "!sub:localhost"] {
            left.mark(&RoomId::parse(id).unwrap());
        }
        let details = space_details(&client, &space, &Named::new(), &left).await;
        assert_eq!(
            rows(&details),
            [
                ("!room:localhost".to_owned(), false, false),
                ("!sub:localhost".to_owned(), false, true)
            ]
        );
        // The space is still ours to change; once left here, it is not.
        assert!(details.can_edit);
        left.mark(space.room_id());
        assert!(
            !space_details(&client, &space, &Named::new(), &left)
                .await
                .can_edit
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_room_or_space_left_here_is_not_changed_as_if_we_were_in_it() {
        const SPACE: &str = "!space:localhost";
        let server = fake::Server::start(json!({ "join": {
                SPACE: fake::joined(true, vec![fake::state(
                    "m.space.child",
                    "!held:localhost",
                    json!({ "via": ["localhost"] }),
                )]),
                "!gone:localhost": fake::joined(false, vec![]),
                "!held:localhost": fake::joined(false, vec![fake::state(
                    "m.space.parent",
                    SPACE,
                    json!({ "via": ["localhost"] }),
                )]),
            } }))
        .await;
        let client = fake::client(&server).await;
        let left = LeftRooms::default();
        for id in ["!gone:localhost", "!held:localhost"] {
            left.mark(&RoomId::parse(id).unwrap());
        }
        // A room left here, though it looks joined, is not put into the space.
        assert_eq!(
            edit_space(&client, &left, SPACE, "!gone:localhost", SpaceEdit::Add).await,
            Err("Could not add the room to the space: you are not in that room.".to_owned())
        );
        assert!(server.sent_state(SPACE).is_empty());
        assert!(server.sent_state("!gone:localhost").is_empty());
        // One that is taken out keeps its own link to the space: it is not ours to
        // change any more.
        assert_eq!(
            edit_space(&client, &left, SPACE, "!held:localhost", SpaceEdit::Remove).await,
            Ok(())
        );
        assert_eq!(server.sent_state(SPACE).len(), 1);
        assert!(server.sent_state("!held:localhost").is_empty());
        // Nothing at all is changed in a space left here.
        left.mark(&RoomId::parse(SPACE).unwrap());
        for edit in [SpaceEdit::Add, SpaceEdit::Remove, SpaceEdit::Suggest(true)] {
            assert_eq!(
                edit_space(&client, &left, SPACE, "!held:localhost", edit).await,
                Err(format!(
                    "Could not {}: you are not in that space.",
                    edit.what()
                ))
            );
        }
        assert_eq!(server.sent_state(SPACE).len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_rooms_link_to_its_space_keeps_being_the_main_one_and_is_not_taken_back_twice() {
        const SPACE: &str = "!space:localhost";
        let parent = |content| fake::state("m.space.parent", SPACE, content);
        let server = fake::Server::start(json!({ "join": {
                SPACE: fake::joined(true, vec![]),
                "!main:localhost": fake::joined(false, vec![parent(
                    json!({ "via": ["localhost"], "canonical": true }),
                )]),
                "!emptied:localhost": fake::joined(false, vec![parent(json!({}))]),
            } }))
        .await;
        let client = fake::client(&server).await;
        let space = get(&client, SPACE);
        let own = client.user_id().unwrap().to_owned();
        // Linked again, it is still the room's main space.
        link_parent(&get(&client, "!main:localhost"), &space, &own, true).await;
        let sent = server.sent_state("!main:localhost");
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            (sent[0].0.as_str(), sent[0].1.as_str()),
            ("m.space.parent", SPACE)
        );
        assert_eq!(sent[0].2["canonical"], json!(true));
        // A link that was taken back already is not taken back again; one that is
        // there is.
        link_parent(&get(&client, "!emptied:localhost"), &space, &own, false).await;
        assert!(server.sent_state("!emptied:localhost").is_empty());
        link_parent(&get(&client, "!main:localhost"), &space, &own, false).await;
        assert_eq!(server.sent_state("!main:localhost")[1].2, json!({}));
    }

    /// A space with one room in it shows up as a space that contains that room.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_space_and_its_room_are_reported() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");

        // Set up with a plain SDK client, independent of what the app does.
        let setup = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .unwrap();
        setup
            .matrix_auth()
            .login_username(&username, &password)
            .initial_device_display_name("matrix-rat space test")
            .send()
            .await
            .unwrap();
        let stamp = std::process::id();
        let mut request = Request::new();
        request.name = Some(format!("Test space {stamp}"));
        let mut content = CreationContent::new();
        content.room_type = Some(RoomType::Space);
        request.creation_content = Some(Raw::new(&content).unwrap());
        let space = setup.create_room(request).await.unwrap();
        let mut request = Request::new();
        request.name = Some(format!("Room in space {stamp}"));
        let room = setup.create_room(request).await.unwrap();
        let via = vec![setup.user_id().unwrap().server_name().to_owned()];
        space
            .send_state_event_for_key(room.room_id(), SpaceChildEventContent::new(via))
            .await
            .unwrap();
        setup.sync_once(SyncSettings::default()).await.unwrap();

        let root = std::env::temp_dir().join(format!("matrix-rat-space-{stamp}"));
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
            homeserver,
            username,
            password: password.into(),
        });
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Spaces(spaces) => {
                        let found = spaces
                            .into_iter()
                            .find(|listed| listed.id == space.room_id().as_str());
                        if let Some(found) = found
                            && found.rooms.iter().any(|id| id == room.room_id().as_str())
                        {
                            return found;
                        }
                    }
                    _ => {}
                }
            }
        };
        let found = tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the space");
        assert_eq!(found.name, format!("Test space {stamp}"));
        assert_eq!(found.level, 0);
        assert_eq!(found.children, [room.room_id().to_string()]);
        assert!(found.can_edit);
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        // Leave no clutter for the other tests.
        let _ = room.leave().await;
        let _ = space.leave().await;
        let _ = setup.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }

    async fn next_manage(events: &mut UnboundedReceiver<MatrixEvent>) -> ManageEvent {
        let wait = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::Manage(event) => return event,
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), wait)
            .await
            .expect("timed out waiting for the answer")
    }

    /// The details of `room_id` once they satisfy `ready`, asking again now and
    /// then: what was just sent reaches the store with the next sync.
    async fn details_when(
        handle: &crate::matrix::MatrixHandle,
        events: &mut UnboundedReceiver<MatrixEvent>,
        room_id: &str,
        ready: impl Fn(&RoomDetails) -> bool,
    ) -> RoomDetails {
        let wait = async {
            loop {
                if let ManageEvent::Info {
                    room_id: about,
                    result,
                } = next_manage(events).await
                    && about == room_id
                    && let Ok(details) = result
                    && ready(&details)
                {
                    return *details;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(60), async {
            handle.send(MatrixCmd::Manage(ManageCmd::Info {
                room_id: room_id.to_owned(),
            }));
            wait.await
        })
        .await
        .expect("the space never looked as expected")
    }

    /// The whole life of a space against a local homeserver, through the worker as
    /// the app drives it: create a space and a room, put the room in, see it listed
    /// as a room of the space, take it out, see it gone, leave both.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=... cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_room_is_put_into_a_space_and_taken_out_again() {
        let homeserver = std::env::var("MATRIX_RAT_TEST_HOMESERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:6167".to_owned());
        let username = std::env::var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = std::env::var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let stamp = std::process::id();
        let root = std::env::temp_dir().join(format!("matrix-rat-space-edit-{stamp}"));
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
            homeserver,
            username,
            password: password.into(),
        });
        let manage = |command: ManageCmd| handle.send(MatrixCmd::Manage(command));
        loop {
            match events.recv().await.expect("worker stopped") {
                MatrixEvent::LoggedIn { .. } => break,
                MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                _ => {}
            }
        }

        manage(ManageCmd::CreateSpace {
            request: 1,
            name: format!("matrix-rat space {stamp}"),
            topic: Some("made by a test".into()),
            alias: None,
        });
        let space_id = loop {
            if let ManageEvent::SpaceCreated { request: 1, result } = next_manage(&mut events).await
            {
                break result.expect("the space was not created");
            }
        };
        manage(ManageCmd::Create {
            request: 2,
            name: format!("matrix-rat room in space {stamp}"),
            topic: None,
            alias: None,
            encrypted: false,
        });
        let room_id = loop {
            if let ManageEvent::Opened { request: 2, result } = next_manage(&mut events).await {
                break result.expect("the room was not created");
            }
        };
        // The space is known as one, and empty, before anything is put into it.
        let details = details_when(&handle, &mut events, &space_id, |d| {
            d.space.as_ref().is_some_and(|space| space.can_edit)
        })
        .await;
        assert!(details.space.unwrap().children.is_empty());

        let edited = async |events: &mut UnboundedReceiver<MatrixEvent>, wanted: u64| loop {
            if let ManageEvent::SpaceEdited {
                request, result, ..
            } = next_manage(events).await
                && request == wanted
            {
                return result;
            }
        };
        manage(ManageCmd::EditSpace {
            request: 3,
            space_id: space_id.clone(),
            child_id: room_id.clone(),
            edit: SpaceEdit::Add,
        });
        edited(&mut events, 3)
            .await
            .expect("the room was not added");
        let details = details_when(&handle, &mut events, &space_id, |d| {
            d.space
                .as_ref()
                .is_some_and(|space| space.children.iter().any(|c| c.room_id == room_id))
        })
        .await;
        let listed = details.space.unwrap().children;
        assert_eq!(listed.len(), 1);
        assert!(listed[0].joined && !listed[0].space && !listed[0].suggested);
        assert_eq!(listed[0].name, format!("matrix-rat room in space {stamp}"));
        assert!(!listed[0].via.is_empty());

        manage(ManageCmd::EditSpace {
            request: 4,
            space_id: space_id.clone(),
            child_id: room_id.clone(),
            edit: SpaceEdit::Remove,
        });
        edited(&mut events, 4)
            .await
            .expect("the room was not removed");
        details_when(&handle, &mut events, &space_id, |d| {
            d.space
                .as_ref()
                .is_some_and(|space| space.children.is_empty())
        })
        .await;

        manage(ManageCmd::StopInfo);
        for id in [&room_id, &space_id] {
            manage(ManageCmd::Leave {
                room_id: id.clone(),
            });
            loop {
                if let ManageEvent::Left { room_id, result } = next_manage(&mut events).await
                    && room_id == *id
                {
                    result.unwrap();
                    break;
                }
            }
        }
        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = std::fs::remove_dir_all(root);
    }
}

/// A homeserver for the tests of the worker's side of spaces, without a server: it
/// answers one sync with the rooms it is given, keeps every request it is sent, and
/// lets a test answer some of them its own way.
#[cfg(test)]
pub(super) mod fake {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use matrix_sdk::Client;
    use matrix_sdk::config::SyncSettings;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Who the client is signed in as.
    pub const ME: &str = "@alice:localhost";
    const SOMEONE: &str = "@bob:localhost";

    /// A request as the server saw it.
    #[derive(Clone, Debug)]
    pub struct Request {
        pub method: String,
        /// Without the query.
        pub path: String,
        pub query: String,
        pub body: String,
    }

    /// A test's own answer to a request.
    pub enum Reply {
        Json(String),
        /// No answer, ever: a server that has gone quiet.
        Never,
    }

    pub type Answer = Box<dyn Fn(&Request) -> Option<Reply> + Send + Sync>;

    pub struct Server {
        pub url: String,
        requests: Arc<Mutex<Vec<Request>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Server {
        /// `rooms`: the `rooms` of the sync it answers.
        pub async fn start(rooms: Value) -> Self {
            Self::answering(rooms, Box::new(|_| None)).await
        }

        /// `start`, where `answer` has the first say on every request.
        pub async fn answering(rooms: Value, answer: Answer) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let answer = Arc::new(answer);
            let sync = json!({ "next_batch": "s1", "rooms": rooms }).to_string();
            let task = tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let seen = seen.clone();
                    let answer = answer.clone();
                    let sync = sync.clone();
                    tokio::spawn(async move {
                        let Some(request) = read(&mut stream).await else {
                            return;
                        };
                        seen.lock().unwrap().push(request.clone());
                        let body = match answer(&request) {
                            Some(Reply::Json(body)) => body,
                            Some(Reply::Never) => return std::future::pending().await,
                            None => usual(&request, &sync),
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self {
                url,
                requests,
                task,
            }
        }

        /// What was asked so far, in order.
        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        /// The state events sent to the room `room_id`, as type, state key and content.
        pub fn sent_state(&self, room_id: &str) -> Vec<(String, String, Value)> {
            let prefix = format!("/_matrix/client/v3/rooms/{room_id}/state/");
            self.requests()
                .into_iter()
                .filter(|request| request.method == "PUT")
                .filter_map(|request| {
                    let rest = request.path.strip_prefix(&prefix)?.to_owned();
                    let (kind, key) = rest.split_once('/').unwrap_or((&rest, ""));
                    let content = serde_json::from_str(&request.body).unwrap_or(Value::Null);
                    Some((kind.to_owned(), unescape(key), content))
                })
                .collect()
        }
    }

    /// One request off the connection, body and all.
    async fn read(stream: &mut tokio::net::TcpStream) -> Option<Request> {
        let mut data = Vec::new();
        let mut chunk = [0u8; 8192];
        let head_end = loop {
            if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            data.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
        let length = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while data.len() < head_end + length {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&chunk[..n]);
        }
        let mut words = head.lines().next()?.split_whitespace();
        let method = words.next()?.to_owned();
        let target = words.next()?;
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        Some(Request {
            method,
            path: unescape(path),
            query: query.to_owned(),
            body: String::from_utf8_lossy(&data[head_end..]).into_owned(),
        })
    }

    /// `%21` and the like, as the client writes room ids into paths.
    fn unescape(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            let hex = (bytes[at] == b'%')
                .then(|| text.get(at + 1..at + 3))
                .flatten()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
            match hex {
                Some(byte) => {
                    out.push(byte);
                    at += 3;
                }
                None => {
                    out.push(bytes[at]);
                    at += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// What a homeserver says that has nothing to tell.
    fn usual(request: &Request, sync: &str) -> String {
        let path = request.path.as_str();
        if path == "/_matrix/client/versions" {
            r#"{"versions":["v1.11"]}"#.to_owned()
        } else if path.ends_with("/sync") {
            sync.to_owned()
        } else if path.ends_with("/keys/upload") {
            r#"{"one_time_key_counts":{}}"#.to_owned()
        } else if path.ends_with("/keys/query") {
            r#"{"device_keys":{}}"#.to_owned()
        } else if request.method == "PUT" && path.contains("/state/") {
            r#"{"event_id":"$sent"}"#.to_owned()
        } else {
            "{}".to_owned()
        }
    }

    /// A client signed in as `ME`, synced once.
    pub async fn client(server: &Server) -> Client {
        use matrix_sdk::authentication::SessionTokens;
        use matrix_sdk::authentication::matrix::MatrixSession;
        let client = Client::builder()
            .homeserver_url(&server.url)
            .build()
            .await
            .unwrap();
        let session = matrix_sdk::AuthSession::Matrix(MatrixSession {
            meta: matrix_sdk::SessionMeta {
                user_id: ME.try_into().unwrap(),
                device_id: "FAKE".into(),
            },
            tokens: SessionTokens {
                access_token: "token".into(),
                refresh_token: None,
            },
        });
        client.restore_session(session).await.unwrap();
        client.sync_once(SyncSettings::default()).await.unwrap();
        client
    }

    /// A state event, sent by `ME` unless `sender` says otherwise.
    pub fn state(kind: &str, key: &str, content: Value) -> Value {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        json!({
            "type": kind,
            "state_key": key,
            "content": content,
            "sender": ME,
            "event_id": format!("$e{n}"),
            "origin_server_ts": n,
        })
    }

    fn create(space: bool, creator: &str) -> Value {
        let mut content = json!({ "creator": creator, "room_version": "10" });
        if space {
            content["type"] = json!("m.space");
        }
        content
    }

    /// A room (or space) someone else invited us to.
    pub fn invited(space: bool) -> Value {
        json!({ "invite_state": { "events": [
            stripped("m.room.create", "", SOMEONE, create(space, SOMEONE)),
            stripped("m.room.member", ME, SOMEONE, json!({ "membership": "invite" })),
        ] } })
    }

    /// A room (or space) we asked to join.
    pub fn knocked(space: bool) -> Value {
        json!({ "knock_state": { "events": [
            stripped("m.room.create", "", SOMEONE, create(space, SOMEONE)),
            stripped("m.room.member", ME, ME, json!({ "membership": "knock" })),
        ] } })
    }

    /// State as an invitation or a request to join shows it.
    fn stripped(kind: &str, key: &str, sender: &str, content: Value) -> Value {
        json!({ "type": kind, "state_key": key, "sender": sender, "content": content })
    }

    /// A room (or space) we were in and are no longer.
    pub fn left(space: bool) -> Value {
        json!({
            "state": { "events": [
                state("m.room.create", "", create(space, ME)),
                state("m.room.member", ME, json!({ "membership": "leave" })),
            ] },
            "timeline": { "events": [], "limited": false },
        })
    }

    /// A room (or space) we made and are in, with `more` of its state.
    pub fn joined(space: bool, more: Vec<Value>) -> Value {
        let mut events = vec![
            state("m.room.create", "", create(space, ME)),
            state("m.room.member", ME, json!({ "membership": "join" })),
        ];
        events.extend(more);
        json!({
            "state": { "events": events },
            "timeline": { "events": [], "limited": false },
        })
    }
}
