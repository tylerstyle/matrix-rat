//! Spaces on `App`: the info of a space (the room info, with the rooms in it),
//! creating one, putting rooms into a space and taking them out. The popups' state is
//! in `crate::manage`; the worker's side in `crate::matrix::spaces`.

use crate::time::Instant;

use super::App;
use super::rooms::{NO_ACCOUNT, OPEN_WITHIN};
use crate::manage::{Confirm, InfoPopup, InfoRow, Pending, SpaceRef};
use crate::matrix::{
    ManageCmd, MemberRole, RoomDetails, RoomHistory, RoomJoinRule, RoomMember, SpaceChild,
    SpaceDetails, SpaceEdit, SpaceSummary,
};
use crate::modal::Modal;
use crate::palette::{Category, InfoBack, Palette, Pick, PickItem};

/// When the space info has no right to show: the rights are the space's admins'.
const NOT_ALLOWED: &str = "You are not allowed to change the rooms of this space.";

impl App {
    // ----- what the room list knows ------------------------------------------------

    /// The space the room list is narrowed to.
    pub(super) fn selected_space(&self) -> Option<&SpaceSummary> {
        self.spaces.get(self.space_at?)
    }

    fn listed_space(&self, id: &str) -> Option<&SpaceSummary> {
        self.spaces.iter().find(|space| space.id == id)
    }

    /// The name of a space as the room list shows it, if it is listed.
    fn space_label(&self, id: &str) -> Option<String> {
        let at = self.spaces.iter().position(|space| space.id == id)?;
        self.space_display_name(at)
    }

    /// Whether we are in a room of a space: the room list knows the rooms, the
    /// spaces the list can be narrowed to, and the worker the deeper ones.
    pub(crate) fn child_joined(&self, child: &SpaceChild) -> bool {
        if child.space {
            self.listed_space(&child.room_id).is_some() || child.joined
        } else {
            self.rooms
                .iter()
                .any(|room| room.id == child.room_id && !room.is_pending())
        }
    }

    /// The spaces `room_id` could be put into: ours to change, not the room itself,
    /// not holding it already. Each once, named as the room list names it.
    fn spaces_for(&self, room_id: &str) -> Vec<PickItem> {
        let mut items: Vec<PickItem> = Vec::new();
        for (at, space) in self.spaces.iter().enumerate() {
            if !space.can_edit
                || space.id == room_id
                || space.children.iter().any(|child| child == room_id)
                || items.iter().any(|item| item.id == space.id)
            {
                continue;
            }
            items.push(PickItem {
                id: space.id.clone(),
                name: self
                    .space_display_name(at)
                    .unwrap_or_else(|| space.name.clone()),
                hint: None,
                category: Category::Space,
            });
        }
        items
    }

    /// What could be put into the space `space_id`: the rooms we are in and our other
    /// spaces, leaving out what it holds already.
    fn rooms_for(&self, space_id: &str, held: &[String]) -> Vec<PickItem> {
        let free = |id: &str| id != space_id && !held.iter().any(|child| child == id);
        let mut items: Vec<PickItem> = crate::model::RoomKind::SECTIONS
            .iter()
            .flat_map(|kind| self.rooms.iter().filter(move |room| room.kind == *kind))
            .filter(|room| !room.is_pending() && free(&room.id))
            .map(|room| PickItem {
                id: room.id.clone(),
                name: room.name.clone(),
                hint: None,
                category: Category::Room,
            })
            .collect();
        for (at, space) in self.spaces.iter().enumerate() {
            if free(&space.id) && !items.iter().any(|item| item.id == space.id) {
                items.push(PickItem {
                    id: space.id.clone(),
                    name: self
                        .space_display_name(at)
                        .unwrap_or_else(|| space.name.clone()),
                    hint: None,
                    category: Category::Space,
                });
            }
        }
        items
    }

    /// Where `room_id` stands with the spaces: the space the list is narrowed to if
    /// the room is directly in it, and whether some space of ours could take it.
    pub(super) fn space_context(&self, room_id: &str) -> (Option<SpaceRef>, bool) {
        let in_space = self
            .selected_space()
            .filter(|space| space.id != room_id && space.children.iter().any(|c| c == room_id))
            .map(|space| SpaceRef {
                id: space.id.clone(),
                // Safe to show wherever the room info and the menus put it.
                name: crate::text::sanitize_line(
                    &self.space_name().unwrap_or_else(|| space.name.clone()),
                ),
                can_edit: space.can_edit,
            });
        (in_space, !self.spaces_for(room_id).is_empty())
    }

    /// Keeps an open room info in step with the spaces. `listed`: whether the space
    /// it shows was in the list before this update; one that has left it closes. The
    /// list goes two levels deep only: a space that is still in a listed one has
    /// moved deeper, it was not left.
    pub(super) fn follow_spaces(&mut self, listed: bool) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        let room_id = info.room_id.clone();
        let deeper = self
            .spaces
            .iter()
            .any(|space| space.children.contains(&room_id));
        if info.space
            && listed
            && !deeper
            && self.listed_space(&room_id).is_none()
            && info.pending != Some(Pending::Leave)
        {
            self.close_manage();
            self.show_toast("You are no longer in that space");
            return;
        }
        let (in_space, can_add) = self.space_context(&room_id);
        if let Some(Modal::RoomInfo(info)) = &mut self.modal {
            info.set_space_context(in_space, can_add);
        }
    }

    /// Selects the space that was just made, once the list has it.
    pub(super) fn select_pending_space(&mut self) {
        let Some((space_id, since)) = &self.manage.pending_space else {
            return;
        };
        if since.elapsed() > OPEN_WITHIN {
            self.manage.pending_space = None;
            return;
        }
        let Some(at) = self.spaces.iter().position(|space| space.id == *space_id) else {
            return;
        };
        self.manage.pending_space = None;
        self.select_space(Some(at));
    }

    // ----- the info of a space -----------------------------------------------------

    /// Shows the info of the space `space_id` (`name` until the details are in).
    pub(super) fn open_space_info(&mut self, space_id: &str, name: &str, select: Option<InfoRow>) {
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        let name = match self.listed_space(space_id) {
            Some(space) => space.name.clone(),
            None if name.is_empty() => "Space".to_owned(),
            None => name.to_owned(),
        };
        let mut popup = InfoPopup::for_space(space_id, &name);
        let (in_space, can_add) = self.space_context(space_id);
        popup.set_space_context(in_space, can_add);
        if self.matrix.is_none() {
            popup.details = Some(self.demo_space_details(space_id, &name));
        }
        if let Some(row) = select {
            popup.select(row);
        }
        self.open_modal(Modal::RoomInfo(Box::new(popup)));
        self.manage(ManageCmd::Info {
            room_id: space_id.to_owned(),
        });
    }

    /// The info of the space the room list is narrowed to.
    pub(super) fn open_selected_space_info(&mut self) {
        match self.selected_space() {
            Some(space) => {
                let (id, name) = (space.id.clone(), space.name.clone());
                self.open_space_info(&id, &name, None);
            }
            None => self.show_toast("No space is selected"),
        }
    }

    /// Shows a room info once more: a space's, or that of a room still in the list.
    pub(super) fn reopen_info(&mut self, back: &InfoBack, select: Option<InfoRow>) {
        if back.space {
            return self.open_space_info(&back.room_id, "", select);
        }
        if let Some(index) = self.rooms.iter().position(|room| room.id == back.room_id) {
            self.open_info(Some(index), select);
        }
    }

    /// Enter on a room of the space: open it if we are in it, else what the room
    /// is, with the way in.
    pub(super) fn open_child(&mut self, row: InfoRow) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        let Some(child) = info.child(row).cloned() else {
            return;
        };
        let joined = self.child_joined(&child);
        if joined && !child.space {
            if let Some(index) = self.rooms.iter().position(|room| room.id == child.room_id) {
                self.close_manage();
                self.open_joined_room_index(index);
                return;
            }
        } else if joined {
            // A space opens as what the room list is narrowed to; one deeper than
            // the list goes shows its info.
            self.close_manage();
            match self
                .spaces
                .iter()
                .position(|space| space.id == child.room_id)
            {
                Some(at) => self.select_space(Some(at)),
                None => self.open_space_info(&child.room_id, &child.name, None),
            }
            return;
        }
        self.open_room_preview(&child.room_id, child.via.clone());
    }

    /// Del (or the " remove " button) on a room of the space: asks first, takes it
    /// out on the second press.
    pub(super) fn remove_child(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        info.notice = None;
        let Some(child) = info.selected_child().cloned() else {
            return;
        };
        if !info.can_edit_children() {
            info.confirm = None;
            info.error = Some(NOT_ALLOWED.to_owned());
            return;
        }
        let question = Confirm::RemoveChild(child.room_id.clone());
        if info.confirm.as_ref() != Some(&question) {
            info.confirm = Some(question);
            return;
        }
        info.confirm = None;
        let space_id = info.room_id.clone();
        self.edit_space(space_id, child.room_id, SpaceEdit::Remove);
    }

    /// Space (or the " suggest " button) on a room of the space: suggested on or off,
    /// shown at once; the server's answer corrects it if need be.
    pub(super) fn toggle_child_suggested(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        info.notice = None;
        info.confirm = None;
        if !info.can_edit_children() {
            info.error = Some(NOT_ALLOWED.to_owned());
            return;
        }
        let Some(InfoRow::Child(index)) = info.current() else {
            return;
        };
        let Some(child) = info
            .details
            .as_mut()
            .and_then(|details| details.space.as_mut())
            .and_then(|space| space.children.get_mut(index))
        else {
            return;
        };
        child.suggested = !child.suggested;
        let (child_id, on) = (child.room_id.clone(), child.suggested);
        let space_id = info.room_id.clone();
        self.edit_space(space_id, child_id, SpaceEdit::Suggest(on));
    }

    /// "Add a room…" of the space info: pick one of the rooms we are in.
    pub(super) fn add_child_from_info(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if !info.can_edit_children() {
            info.error = Some("You are not allowed to add rooms to this space.".to_owned());
            return;
        }
        let held: Vec<String> = info
            .children()
            .iter()
            .map(|child| child.room_id.clone())
            .collect();
        let (space_id, space_name) = (info.room_id.clone(), info.name.clone());
        self.open_room_picker(space_id, space_name, &held, true);
    }

    /// "Remove from …" of a room's info, asked and answered: takes the room out of
    /// the space the room list is narrowed to.
    pub(super) fn remove_from_selected_space(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        let Some(space) = info.in_space.clone() else {
            return;
        };
        if !space.can_edit {
            info.error = Some(format!(
                "You are not allowed to take rooms out of {}.",
                space.name
            ));
            return;
        }
        let room_id = info.room_id.clone();
        self.edit_space(space.id, room_id, SpaceEdit::Remove);
    }

    // ----- pickers -------------------------------------------------------------------

    /// The rooms that could be put into the space `space_id`, to pick one. `back`:
    /// the space's info was open and is shown again afterwards.
    fn open_room_picker(
        &mut self,
        space_id: String,
        space_name: String,
        held: &[String],
        back: bool,
    ) {
        let items = self.rooms_for(&space_id, held);
        if items.is_empty() {
            return self.tell("Every room you are in is in this space already.");
        }
        if back {
            self.close_manage();
        }
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        let pick = Pick::IntoSpace {
            back: back.then(|| InfoBack {
                room_id: space_id.clone(),
                space: true,
            }),
            space_id,
            space_name,
        };
        self.open_modal(Modal::Palette(Palette::open_pick(&items, pick)));
    }

    /// The spaces the room `room_id` could be put into, to pick one. `back`: the room
    /// info to show again afterwards.
    fn open_space_picker(&mut self, room_id: String, room_name: String, back: Option<InfoBack>) {
        let items = self.spaces_for(&room_id);
        if items.is_empty() {
            return self.tell(if self.spaces.is_empty() {
                "You are not in any space."
            } else {
                "None of your spaces that you may change can take this room."
            });
        }
        if back.is_some() {
            self.close_manage();
        }
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        let pick = Pick::ToSpace {
            back,
            room_id,
            room_name,
        };
        self.open_modal(Modal::Palette(Palette::open_pick(&items, pick)));
    }

    /// "Add to a space…" for the room the room-level commands are about.
    pub(super) fn add_target_room_to_space(&mut self) {
        let Some(room) = self.target_room().and_then(|index| self.rooms.get(index)) else {
            self.show_toast("No room is open");
            return;
        };
        if room.is_pending() {
            self.show_toast(room.kind.pending_hint());
            return;
        }
        let (room_id, name) = (room.id.clone(), room.name.clone());
        self.open_space_picker(room_id, name, None);
    }

    /// "Add to a space…" in a room's info.
    pub(super) fn add_to_space_from_info(&mut self) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        let back = InfoBack {
            room_id: info.room_id.clone(),
            space: info.space,
        };
        let (room_id, name) = (info.room_id.clone(), info.name.clone());
        self.open_space_picker(room_id, name, Some(back));
    }

    /// "Add a room…" for the space the room list is narrowed to.
    pub(super) fn add_room_to_selected_space(&mut self) {
        let Some(space) = self.selected_space() else {
            self.show_toast("No space is selected");
            return;
        };
        if !space.can_edit {
            self.show_toast("You are not allowed to add rooms to this space");
            return;
        }
        let (space_id, held) = (space.id.clone(), space.children.clone());
        let name = self.space_name().unwrap_or_else(|| space.name.clone());
        self.open_room_picker(space_id, name, &held, false);
    }

    /// A room (or space) was picked in the palette.
    pub(super) fn picked(&mut self, pick: Pick, picked: String) {
        match pick {
            Pick::Forward(target) => self.forward_to_room(target, picked),
            Pick::IntoSpace { space_id, back, .. } => {
                if let Some(back) = back {
                    self.reopen_info(&back, Some(InfoRow::AddChild));
                }
                self.edit_space(space_id, picked, SpaceEdit::Add);
            }
            Pick::ToSpace { room_id, back, .. } => {
                if let Some(back) = back {
                    self.reopen_info(&back, Some(InfoRow::AddToSpace));
                }
                self.edit_space(picked, room_id, SpaceEdit::Add);
            }
        }
    }

    /// Says something in the open room info, or as a toast where there is none.
    fn tell(&mut self, text: &str) {
        match &mut self.modal {
            Some(Modal::RoomInfo(info)) => info.error = Some(text.to_owned()),
            _ => self.show_toast(text),
        }
    }

    // ----- changes and their answers -------------------------------------------------

    /// Sends a change of a space. An open room info is what asked: it waits for the
    /// answer. Without one, the answer comes as a toast.
    fn edit_space(&mut self, space_id: String, child_id: String, edit: SpaceEdit) {
        if self.matrix.is_none() {
            let result = self.demo_edit_space(&space_id, &child_id, edit);
            return self.space_edited(true, result);
        }
        let request = self.next_request();
        if let Some(Modal::RoomInfo(info)) = &mut self.modal {
            info.error = None;
            info.notice = None;
            let busy = match edit {
                SpaceEdit::Add => "Adding…",
                SpaceEdit::Remove => "Removing…",
                SpaceEdit::Suggest(_) => "Saving…",
            };
            info.wait_for(Pending::Space(request), busy);
        }
        self.manage(ManageCmd::EditSpace {
            request,
            space_id,
            child_id,
            edit,
        });
    }

    pub(super) fn on_space_edited(
        &mut self,
        request: u64,
        space_id: &str,
        child_id: &str,
        edit: SpaceEdit,
        result: Result<(), String>,
    ) {
        // Only the view that asked takes the answer; any other just hears of it.
        let asked = match &mut self.modal {
            Some(Modal::RoomInfo(info)) => info.answered(Pending::Space(request)),
            _ => false,
        };
        let result = result.map(|()| self.space_edit_text(space_id, child_id, edit));
        self.space_edited(asked, result);
    }

    fn space_edited(&mut self, asked: bool, result: Result<String, String>) {
        let info = match &mut self.modal {
            Some(Modal::RoomInfo(info)) if asked => Some(info),
            _ => None,
        };
        match (result, info) {
            (Ok(done), Some(info)) => {
                info.notice = Some(done.clone());
                self.show_toast(done);
            }
            (Ok(done), None) => self.show_toast(done),
            (Err(error), Some(info)) => {
                info.error = Some(error);
                // What was shown ahead of the answer is not true; look again.
                let room_id = info.room_id.clone();
                self.manage(ManageCmd::Info { room_id });
            }
            (Err(error), None) => self.show_toast(error),
        }
    }

    /// "Added rust to Work" and the like.
    fn space_edit_text(&self, space_id: &str, child_id: &str, edit: SpaceEdit) -> String {
        let room = self
            .name_of(child_id)
            .unwrap_or_else(|| "the room".to_owned());
        let space = self
            .name_of(space_id)
            .unwrap_or_else(|| "the space".to_owned());
        match edit {
            SpaceEdit::Add => format!("Added {room} to {space}"),
            SpaceEdit::Remove => format!("Took {room} out of {space}"),
            SpaceEdit::Suggest(true) => format!("{room} is suggested in {space}"),
            SpaceEdit::Suggest(false) => format!("{room} is no longer suggested in {space}"),
        }
    }

    /// The name of a room or space, from wherever it is known.
    fn name_of(&self, id: &str) -> Option<String> {
        if let Some(room) = self.rooms.iter().find(|room| room.id == id) {
            return Some(room.name.clone());
        }
        if let Some(name) = self.space_label(id) {
            return Some(name);
        }
        match &self.modal {
            Some(Modal::RoomInfo(info)) if info.room_id == id => Some(info.name.clone()),
            Some(Modal::RoomInfo(info)) => info
                .children()
                .iter()
                .find(|child| child.room_id == id && !child.name.is_empty())
                .map(|child| child.name.clone()),
            _ => None,
        }
    }

    pub(super) fn on_space_created(&mut self, request: u64, result: Result<String, String>) {
        let asked = match &mut self.modal {
            Some(Modal::New(popup)) if popup.request == Some(request) => {
                popup.request = None;
                popup.busy = None;
                true
            }
            _ => false,
        };
        match (result, asked) {
            (Ok(space_id), true) => {
                self.close_manage();
                self.show_toast("Space created");
                self.manage.pending_space = Some((space_id, Instant::now()));
                self.select_pending_space();
            }
            (Ok(_), false) => self.show_toast("Done: the space is in your list of spaces"),
            (Err(error), true) => {
                if let Some(Modal::New(popup)) = &mut self.modal {
                    popup.error = Some(error);
                }
            }
            (Err(error), false) => self.show_toast(error),
        }
    }

    // ----- the demo --------------------------------------------------------------------

    /// A space for the demo to show: three of its rooms, and one it is not in.
    pub fn add_demo_space(&mut self) {
        let children: Vec<String> = ["rust", "matrix-rat", "nixos", DEMO_OUTSIDE.0]
            .into_iter()
            .map(str::to_owned)
            .collect();
        self.spaces = vec![SpaceSummary {
            id: DEMO_SPACE.to_owned(),
            name: "Rat club".to_owned(),
            level: 0,
            rooms: children.clone(),
            children,
            can_edit: true,
        }];
        self.manage.demo_suggested = vec![(DEMO_SPACE.to_owned(), "matrix-rat".to_owned())];
        let at = self.space_at;
        self.set_space(at);
    }

    /// The demo has no server to ask: its space is what the room list says, with
    /// everyone who wrote in its rooms as members.
    fn demo_space_details(&self, space_id: &str, name: &str) -> RoomDetails {
        let children = self
            .listed_space(space_id)
            .map(|space| space.children.clone())
            .unwrap_or_default();
        let member = |name: &str, own: bool, admin: bool| RoomMember {
            user_id: format!("@{name}:example.org"),
            name: name.to_owned(),
            role: if admin {
                MemberRole::Admin
            } else {
                MemberRole::User
            },
            power_level: if admin { 100 } else { 0 },
            own,
            ..RoomMember::default()
        };
        let children: Vec<SpaceChild> = children
            .iter()
            .map(|id| {
                let name = self.name_of(id).unwrap_or_else(|| {
                    let told = if id == DEMO_OUTSIDE.0 {
                        DEMO_OUTSIDE.1
                    } else {
                        ""
                    };
                    told.to_owned()
                });
                let mut child = SpaceChild {
                    room_id: id.clone(),
                    name,
                    space: self.listed_space(id).is_some(),
                    suggested: self
                        .manage
                        .demo_suggested
                        .iter()
                        .any(|(space, child)| space == space_id && child == id),
                    via: vec!["example.org".to_owned()],
                    ..SpaceChild::default()
                };
                child.joined = self.child_joined(&child);
                child
            })
            .collect();
        RoomDetails {
            name: name.to_owned(),
            topic: "Demo data. Log in to see your own spaces.".to_owned(),
            alias: Some("#rat-club:example.org".to_owned()),
            joined: 3,
            join_rule: Some(RoomJoinRule::Invite),
            history_visibility: Some(RoomHistory::Shared),
            members: vec![
                member(crate::model::OWN_NAME, true, true),
                member("alice", false, false),
                member("bob", false, false),
            ],
            space: Some(SpaceDetails {
                children,
                more: false,
                can_edit: true,
            }),
            ..RoomDetails::default()
        }
    }

    /// The demo changes its own space, so that the feature can be tried out.
    fn demo_edit_space(
        &mut self,
        space_id: &str,
        child_id: &str,
        edit: SpaceEdit,
    ) -> Result<String, String> {
        let done = self.space_edit_text(space_id, child_id, edit);
        let Some(space) = self.spaces.iter_mut().find(|space| space.id == space_id) else {
            return Err(NO_ACCOUNT.to_owned());
        };
        match edit {
            SpaceEdit::Add => {
                if !space.children.iter().any(|child| child == child_id) {
                    space.children.push(child_id.to_owned());
                    space.rooms.push(child_id.to_owned());
                }
            }
            SpaceEdit::Remove => {
                space.children.retain(|child| child != child_id);
                space.rooms.retain(|room| room != child_id);
                self.manage
                    .demo_suggested
                    .retain(|(space, child)| space != space_id || child != child_id);
            }
            SpaceEdit::Suggest(on) => {
                self.manage
                    .demo_suggested
                    .retain(|(space, child)| space != space_id || child != child_id);
                if on {
                    self.manage
                        .demo_suggested
                        .push((space_id.to_owned(), child_id.to_owned()));
                }
            }
        }
        // The room list narrowed to that space, and an open info, show it at once.
        let at = self.space_at;
        self.set_space(at);
        let shown = match &self.modal {
            Some(Modal::RoomInfo(info)) if info.room_id == space_id => Some(info.name.clone()),
            _ => None,
        };
        if let Some(name) = shown {
            let details = self.demo_space_details(space_id, &name);
            if let Some(Modal::RoomInfo(info)) = &mut self.modal {
                info.set_details(details);
            }
        }
        self.follow_spaces(true);
        Ok(done)
    }
}

/// The demo's space, and the one room in it that the demo is not in.
const DEMO_SPACE: &str = "!ratclub:example.org";
const DEMO_OUTSIDE: (&str, &str) = ("!burrow:example.org", "burrow");

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::input::{
        InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use crate::manage::{Click, NewPopup, PreviewButton, Stage};
    use crate::matrix::{
        ManageEvent, MatrixCmd, MatrixEvent, MatrixHandle, PreviewJoinRule, PreviewMembership,
        RoomCategory, RoomPreview, RoomSummary,
    };
    use crate::ui;

    const SPACE: &str = "!work:example.org";
    const FUN: &str = "!fun:example.org";
    const RATS: &str = "!rats:example.org";
    const CHEESE: &str = "!cheese:example.org";
    const OUTSIDE: &str = "!outside:example.org";

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn find(text: &str, needle: &str) -> (u16, u16) {
        for (row, line) in text.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                let col = crate::text::width(&line[..byte]) - 1;
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{text}");
    }

    fn room(id: &str, name: &str) -> RoomSummary {
        RoomSummary {
            encrypted: false,
            id: id.into(),
            name: name.into(),
            category: RoomCategory::Room,
            notifications: 0,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            preview: None,
            avatar: None,
            alias: None,
            direct: false,
            muted: false,
        }
    }

    fn space(id: &str, name: &str, children: &[&str], can_edit: bool) -> SpaceSummary {
        let children: Vec<String> = children.iter().map(|id| (*id).to_owned()).collect();
        SpaceSummary {
            id: id.into(),
            name: name.into(),
            level: 0,
            rooms: children.clone(),
            children,
            can_edit,
        }
    }

    /// An app with an account, two rooms, the space Work (Rats in it, and a room we
    /// are not in) that we may change, and Fun, which we may not.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Rooms(vec![
            room(RATS, "Rats"),
            room(CHEESE, "Cheese"),
        ]));
        app.on_matrix(MatrixEvent::Spaces(vec![
            space(SPACE, "Work", &[RATS, OUTSIDE], true),
            space(FUN, "Fun", &[], false),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal, commands)
    }

    /// The room management commands sent since the last call.
    fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<ManageCmd> {
        let mut out = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::Manage(command) = command {
                out.push(command);
            }
        }
        out
    }

    fn described(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        sent(commands).iter().map(ManageCmd::describe).collect()
    }

    fn answer(app: &mut App, event: ManageEvent) {
        app.on_matrix(MatrixEvent::Manage(event));
    }

    fn info(app: &App) -> &InfoPopup {
        match &app.modal {
            Some(Modal::RoomInfo(info)) => info,
            other => panic!("room info is not open: {other:?}"),
        }
    }

    fn details(can_edit: bool) -> RoomDetails {
        let me = RoomMember {
            user_id: "@me:example.org".into(),
            name: "me".into(),
            role: MemberRole::Admin,
            power_level: 100,
            own: true,
            ..RoomMember::default()
        };
        RoomDetails {
            name: "Work".into(),
            topic: "Where the work happens".into(),
            joined: 1,
            can_rename: can_edit,
            can_set_topic: can_edit,
            can_invite: true,
            notify: Some(crate::matrix::NotifyLevel::All),
            members: vec![me],
            space: Some(SpaceDetails {
                children: vec![
                    SpaceChild {
                        room_id: RATS.into(),
                        name: "Rats".into(),
                        joined: true,
                        via: vec!["example.org".into()],
                        ..SpaceChild::default()
                    },
                    SpaceChild {
                        room_id: OUTSIDE.into(),
                        name: "Outside".into(),
                        suggested: true,
                        via: vec!["elsewhere.org".into()],
                        ..SpaceChild::default()
                    },
                ],
                more: false,
                can_edit,
            }),
            ..RoomDetails::default()
        }
    }

    /// Opens the info of Work from the palette's way in and delivers its details.
    fn with_space_info(app: &mut App, commands: &mut UnboundedReceiver<MatrixCmd>, can_edit: bool) {
        app.select_space(Some(0));
        sent(commands);
        app.perform(Action::SpaceInfo);
        assert_eq!(described(commands), [format!("Info({SPACE})")]);
        answer(
            app,
            ManageEvent::Info {
                room_id: SPACE.into(),
                result: Ok(Box::new(details(can_edit))),
            },
        );
    }

    fn select(app: &mut App, row: InfoRow) {
        if let Some(Modal::RoomInfo(info)) = &mut app.modal {
            info.select(row);
        }
        assert_eq!(info(app).current(), Some(row));
    }

    fn last_request(app: &App) -> u64 {
        app.manage.requests_for_test()
    }

    #[test]
    fn creating_a_space_selects_it_once_the_list_has_it() {
        let (mut app, _terminal, mut commands) = connected();
        app.on_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        app.on_event(key(KeyCode::Char('4')));
        let form = |app: &App| match &app.modal {
            Some(Modal::New(popup)) => popup.as_ref().clone(),
            other => panic!("the new popup is not open: {other:?}"),
        };
        assert_eq!(form(&app).stage, Stage::CreateSpace);
        // A private space has nothing to say where a room has its encryption switch:
        // from the access switch the cursor goes on to the button.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        assert_eq!(form(&app).focus, NewPopup::ACCESS);
        app.on_event(key(KeyCode::Down));
        assert_eq!(form(&app).focus, NewPopup::BUTTON);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(form(&app).error.as_deref(), Some("Give the space a name."));
        // Public needs an address.
        app.perform(Action::Manage(Click::Flip(NewPopup::ACCESS)));
        app.perform(Action::Manage(Click::Slot(NewPopup::NAME)));
        type_text(&mut app, "Club");
        app.perform(Action::Manage(Click::Submit));
        assert_eq!(
            form(&app).error.as_deref(),
            Some("A public space needs an address.")
        );
        assert!(sent(&mut commands).is_empty());
        app.perform(Action::Manage(Click::Flip(NewPopup::ACCESS)));
        app.perform(Action::Manage(Click::Submit));
        let request = last_request(&app);
        assert_eq!(
            described(&mut commands),
            [format!("CreateSpace(request {request}, public: false)")]
        );
        assert_eq!(form(&app).busy, Some("Creating the space…"));
        // Made: the popup closes, and the space is selected as soon as it is listed.
        answer(
            &mut app,
            ManageEvent::SpaceCreated {
                request,
                result: Ok("!club:example.org".into()),
            },
        );
        assert!(app.modal.is_none());
        assert!(app.space.is_none());
        app.on_matrix(MatrixEvent::Spaces(vec![
            space(SPACE, "Work", &[RATS], true),
            space("!club:example.org", "Club", &[], true),
        ]));
        assert_eq!(app.space.as_deref(), Some("!club:example.org"));
        assert_eq!(app.space_name().as_deref(), Some("Club"));
    }

    #[test]
    fn a_space_shows_its_rooms_and_leaves_out_what_only_fits_a_chat() {
        let (mut app, mut terminal, mut commands) = connected();
        with_space_info(&mut app, &mut commands, true);
        let rows = info(&app).rows();
        for gone in [
            InfoRow::Favourite,
            InfoRow::LowPriority,
            InfoRow::Pinned,
            InfoRow::Notify,
            InfoRow::OwnName,
        ] {
            assert!(!rows.contains(&gone), "{gone:?}");
        }
        for shown in [
            InfoRow::Name,
            InfoRow::Invite,
            InfoRow::Leave,
            InfoRow::AddChild,
            InfoRow::Child(0),
            InfoRow::Child(1),
            InfoRow::Member(0),
        ] {
            assert!(rows.contains(&shown), "{shown:?}");
        }
        if let Some(Modal::RoomInfo(info)) = &mut app.modal {
            info.settings_open = true;
        }
        let settings: Vec<InfoRow> = info(&app)
            .rows()
            .into_iter()
            .filter(|row| matches!(row, InfoRow::Setting(_)))
            .collect();
        assert!(!settings.contains(&InfoRow::Setting(crate::manage::RoomSetting::Encryption)));
        assert!(!settings.contains(&InfoRow::Setting(crate::manage::RoomSetting::Direct)));
        assert!(settings.contains(&InfoRow::Setting(crate::manage::RoomSetting::JoinRule)));
        let text = screen(&mut app, &mut terminal);
        for needle in [
            "Space info",
            "Space ID",
            "Rooms (2)",
            "Add a room…",
            "Rats",
            "Outside",
            "suggested · not joined",
            "Leave space",
        ] {
            find(&text, needle);
        }
        assert!(!text.contains("Encryption"));
    }

    #[test]
    fn adding_a_room_needs_the_right_and_comes_back_to_the_space() {
        let (mut app, _terminal, mut commands) = connected();
        with_space_info(&mut app, &mut commands, false);
        select(&mut app, InfoRow::AddChild);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            info(&app).error.as_deref(),
            Some("You are not allowed to add rooms to this space.")
        );
        assert!(sent(&mut commands).is_empty());

        app.on_event(key(KeyCode::Esc));
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::AddChild);
        app.on_event(key(KeyCode::Enter));
        // The picker offers the rooms we are in that are not in the space yet.
        let titles = match &app.modal {
            Some(Modal::Palette(palette)) => {
                assert!(matches!(palette.pick, Some(Pick::IntoSpace { .. })));
                palette
                    .entries
                    .iter()
                    .map(|entry| entry.title.clone())
                    .collect::<Vec<_>>()
            }
            other => panic!("no picker: {other:?}"),
        };
        assert_eq!(titles, ["Cheese", "Fun"]);
        assert_eq!(described(&mut commands), ["StopInfo"]);
        // Esc goes back to the space, on the row it came from (once it is there).
        app.on_event(key(KeyCode::Esc));
        assert_eq!(described(&mut commands), [format!("Info({SPACE})")]);
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: SPACE.into(),
                result: Ok(Box::new(details(true))),
            },
        );
        assert_eq!(info(&app).current(), Some(InfoRow::AddChild));

        app.on_event(key(KeyCode::Enter));
        sent(&mut commands);
        type_text(&mut app, "chee");
        app.on_event(key(KeyCode::Enter));
        let request = last_request(&app);
        assert_eq!(
            described(&mut commands),
            [
                format!("Info({SPACE})"),
                format!("EditSpace(request {request}, {SPACE}, {CHEESE}, Add)")
            ]
        );
        assert_eq!(info(&app).room_id, SPACE);
        assert_eq!(info(&app).busy, Some("Adding…"));
        answer(
            &mut app,
            ManageEvent::SpaceEdited {
                request,
                space_id: SPACE.into(),
                child_id: CHEESE.into(),
                edit: SpaceEdit::Add,
                result: Ok(()),
            },
        );
        assert!(info(&app).busy.is_none());
        assert_eq!(info(&app).notice.as_deref(), Some("Added Cheese to Work"));
    }

    #[test]
    fn a_click_beside_the_picker_goes_back_to_the_space_as_esc_does() {
        let (mut app, mut terminal, mut commands) = connected();
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::AddChild);
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        screen(&mut app, &mut terminal);
        sent(&mut commands);
        app.on_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(info(&app).room_id, SPACE);
        assert_eq!(described(&mut commands), [format!("Info({SPACE})")]);
        // On the row it came from, once that is there.
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: SPACE.into(),
                result: Ok(Box::new(details(true))),
            },
        );
        assert_eq!(info(&app).current(), Some(InfoRow::AddChild));
    }

    #[test]
    fn taking_a_room_out_of_a_space_asks_first() {
        let (mut app, mut terminal, mut commands) = connected();
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::Child(0));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(info(&app).confirm, Some(Confirm::RemoveChild(RATS.into())));
        find(
            &screen(&mut app, &mut terminal),
            "Del again to remove, Esc to keep",
        );
        assert!(sent(&mut commands).is_empty());
        // Esc takes the question back.
        app.on_event(key(KeyCode::Esc));
        assert!(info(&app).confirm.is_none());
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        let request = last_request(&app);
        assert_eq!(
            described(&mut commands),
            [format!(
                "EditSpace(request {request}, {SPACE}, {RATS}, Remove)"
            )]
        );
        assert_eq!(info(&app).busy, Some("Removing…"));
        // A refusal is told in words, and the view asks again for what is true.
        answer(
            &mut app,
            ManageEvent::SpaceEdited {
                request,
                space_id: SPACE.into(),
                child_id: RATS.into(),
                edit: SpaceEdit::Remove,
                result: Err("Could not remove the room from the space: not allowed".into()),
            },
        );
        assert_eq!(
            info(&app).error.as_deref(),
            Some("Could not remove the room from the space: not allowed")
        );
        assert_eq!(described(&mut commands), [format!("Info({SPACE})")]);

        // Without the right there is no question, only the reason.
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: SPACE.into(),
                result: Ok(Box::new(details(false))),
            },
        );
        select(&mut app, InfoRow::Child(0));
        app.on_event(key(KeyCode::Delete));
        assert!(info(&app).confirm.is_none());
        assert_eq!(info(&app).error.as_deref(), Some(NOT_ALLOWED));
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn suggested_is_shown_at_once_and_late_answers_touch_only_who_asked() {
        let (mut app, _terminal, mut commands) = connected();
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::Child(0));
        app.on_event(key(KeyCode::Char(' ')));
        let request = last_request(&app);
        assert!(info(&app).children()[0].suggested);
        assert_eq!(
            described(&mut commands),
            [format!(
                "EditSpace(request {request}, {SPACE}, {RATS}, Suggest(true))"
            )]
        );
        // A space typed into the member filter is part of the filter, even while
        // nobody matches and the selection stays on a room of the space.
        type_text(&mut app, "q z");
        assert_eq!(info(&app).filter.as_str(), "q z");
        assert_eq!(info(&app).current(), Some(InfoRow::Child(0)));
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));
        assert!(info(&app).filter.is_empty());
        // An answer to some other request is not this view's.
        let other = |request| ManageEvent::SpaceEdited {
            request,
            space_id: SPACE.into(),
            child_id: RATS.into(),
            edit: SpaceEdit::Suggest(true),
            result: Ok(()),
        };
        answer(&mut app, other(request + 7));
        assert_eq!(info(&app).busy, Some("Saving…"));
        answer(&mut app, other(request));
        assert!(info(&app).busy.is_none());
        assert_eq!(
            info(&app).notice.as_deref(),
            Some("Rats is suggested in Work")
        );
        // Once the view is gone, a late answer only shows as a toast.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        answer(
            &mut app,
            ManageEvent::SpaceEdited {
                request,
                space_id: SPACE.into(),
                child_id: RATS.into(),
                edit: SpaceEdit::Remove,
                result: Err("Could not remove the room from the space: not allowed".into()),
            },
        );
        assert!(app.modal.is_none());
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("Could not remove the room from the space: not allowed")
        );
    }

    #[test]
    fn del_takes_a_room_out_only_while_the_room_is_in_view() {
        let (mut app, _terminal, mut commands) = connected();
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::Child(0));
        // Nobody is called "zz": the filter's line is in view, and Del edits it.
        type_text(&mut app, "zz");
        assert!(info(&app).typing_blind());
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(info(&app).filter.as_str(), "z");
        assert!(info(&app).confirm.is_none());
        // Chosen with the arrows, the room is in view, and Del asks; not with Ctrl.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Up));
        assert_eq!(info(&app).current(), Some(InfoRow::Child(0)));
        assert!(!info(&app).typing_blind());
        app.on_event(InputEvent::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::CONTROL,
        )));
        assert!(info(&app).confirm.is_none());
        app.on_event(key(KeyCode::Delete));
        assert_eq!(info(&app).confirm, Some(Confirm::RemoveChild(RATS.into())));
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn a_room_of_a_space_opens_or_is_previewed() {
        let (mut app, _terminal, mut commands) = connected();
        app.open_room(1);
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::Child(0));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.rooms[app.active].id, RATS);

        sent(&mut commands);
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::Child(1));
        app.on_event(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::New(popup)) => {
                assert_eq!(popup.stage, Stage::Preview);
                assert_eq!(popup.preview_target, OUTSIDE);
                assert_eq!(popup.preview_via, ["elsewhere.org"]);
            }
            other => panic!("no preview: {other:?}"),
        }
        let commands = described(&mut commands);
        assert!(
            commands.last().is_some_and(|c| c.starts_with("Preview(")),
            "{commands:?}"
        );
    }

    fn preview_of(id: &str, space: bool, membership: PreviewMembership) -> RoomPreview {
        RoomPreview {
            room_id: id.into(),
            canonical_alias: None,
            name: Some("Inner".into()),
            topic: None,
            avatar: None,
            joined_members: 2,
            join_rule: PreviewJoinRule::Public,
            encrypted: None,
            world_readable: false,
            membership,
            via: vec!["example.org".into()],
            space,
        }
    }

    /// Previews `id` and delivers the server's answer.
    fn previewed(app: &mut App, id: &str, space: bool, membership: PreviewMembership) {
        app.open_room_preview(id, Vec::new());
        let request = last_request(app);
        answer(
            app,
            ManageEvent::Preview {
                request,
                result: Ok(Box::new(preview_of(id, space, membership))),
            },
        );
    }

    fn preview_buttons(app: &App) -> Vec<PreviewButton> {
        match &app.modal {
            Some(Modal::New(popup)) => popup.preview_buttons(),
            other => panic!("no preview: {other:?}"),
        }
    }

    #[test]
    fn a_space_from_a_preview_is_found_among_the_spaces() {
        let (mut app, _terminal, mut commands) = connected();
        // A space we are in, though the answer does not say so: Open selects it.
        previewed(&mut app, FUN, true, PreviewMembership::None);
        assert_eq!(
            preview_buttons(&app),
            [PreviewButton::Open, PreviewButton::Cancel]
        );
        app.perform(Action::Manage(Click::PreviewBtn(0)));
        assert!(app.modal.is_none());
        assert_eq!(app.space.as_deref(), Some(FUN));

        // One deeper than the list goes shows its info.
        previewed(
            &mut app,
            "!inner:example.org",
            true,
            PreviewMembership::Joined,
        );
        sent(&mut commands);
        app.perform(Action::Manage(Click::PreviewBtn(0)));
        assert!(info(&app).space);
        assert_eq!(info(&app).room_id, "!inner:example.org");
        assert_eq!(described(&mut commands), ["Info(!inner:example.org)"]);
        app.on_event(key(KeyCode::Esc));

        // Joined from its preview, it is selected once it is listed.
        previewed(&mut app, "!club:example.org", true, PreviewMembership::None);
        assert_eq!(
            preview_buttons(&app),
            [PreviewButton::Join, PreviewButton::Cancel]
        );
        sent(&mut commands);
        app.perform(Action::Manage(Click::PreviewBtn(0)));
        let request = last_request(&app);
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Join { target, .. }] if target == "!club:example.org"
        ));
        answer(
            &mut app,
            ManageEvent::Opened {
                request,
                result: Ok("!club:example.org".into()),
            },
        );
        assert!(app.modal.is_none());
        app.on_matrix(MatrixEvent::Spaces(vec![
            space(SPACE, "Work", &[RATS, OUTSIDE], true),
            space(FUN, "Fun", &[], false),
            space("!club:example.org", "Club", &[], false),
        ]));
        assert_eq!(app.space.as_deref(), Some("!club:example.org"));
    }

    #[test]
    fn changed_spaces_from_the_worker_are_followed() {
        let (mut app, _terminal, mut commands) = connected();
        app.select_space(Some(0));
        // Cheese is not in Work: nothing to take it out of.
        app.open_info(Some(1), None);
        assert!(info(&app).in_space.is_none());
        assert!(info(&app).can_add_to_space);
        assert!(!info(&app).rows().contains(&InfoRow::RemoveFromSpace));
        app.on_matrix(MatrixEvent::Spaces(vec![
            space(SPACE, "Work", &[RATS, CHEESE], true),
            space(FUN, "Fun", &[], false),
        ]));
        assert_eq!(
            info(&app).in_space.as_ref().map(|space| space.id.as_str()),
            Some(SPACE)
        );
        assert!(info(&app).rows().contains(&InfoRow::RemoveFromSpace));
        // In every space it could be in: nowhere left to put it.
        assert!(!info(&app).can_add_to_space);
        app.on_event(key(KeyCode::Esc));

        // A space put into a space of the second level is deeper than the list goes:
        // it is not listed any more, but it was not left either.
        sent(&mut commands);
        with_space_info(&mut app, &mut commands, true);
        let mut deeper = space(FUN, "Fun", &["!inner:example.org"], false);
        deeper.level = 1;
        let mut inner = space("!inner:example.org", "Inner", &[SPACE], false);
        inner.level = 1;
        app.on_matrix(MatrixEvent::Spaces(vec![deeper, inner]));
        assert_eq!(info(&app).room_id, SPACE);
        app.on_event(key(KeyCode::Esc));
        app.on_matrix(MatrixEvent::Spaces(vec![
            space(SPACE, "Work", &[RATS], true),
            space(FUN, "Fun", &[], false),
        ]));

        // The info of a space we are no longer in closes.
        sent(&mut commands);
        with_space_info(&mut app, &mut commands, true);
        app.on_matrix(MatrixEvent::Spaces(vec![space(FUN, "Fun", &[], false)]));
        assert!(app.modal.is_none());
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("You are no longer in that space")
        );
    }

    #[test]
    fn the_info_of_a_space_we_are_no_longer_in_closes_however_deep_it_is() {
        const INNER: &str = "!inner:example.org";
        let (mut app, _terminal, mut commands) = connected();
        // Inner is deeper than the list goes, and Work still names it after we left.
        let spaces = || {
            vec![
                space(SPACE, "Work", &[RATS, INNER], true),
                space(FUN, "Fun", &[], false),
            ]
        };
        app.on_matrix(MatrixEvent::Spaces(spaces()));
        app.open_space_info(INNER, "Inner", None);
        app.on_matrix(MatrixEvent::Spaces(spaces()));
        assert_eq!(info(&app).room_id, INNER);
        // Another room's news is not this view's; nor is our own leaving, which its
        // answer ends.
        answer(
            &mut app,
            ManageEvent::Gone {
                room_id: RATS.into(),
            },
        );
        if let Some(Modal::RoomInfo(info)) = &mut app.modal {
            info.wait_for(Pending::Leave, "Leaving…");
        }
        answer(
            &mut app,
            ManageEvent::Gone {
                room_id: INNER.into(),
            },
        );
        assert_eq!(info(&app).room_id, INNER);
        if let Some(Modal::RoomInfo(info)) = &mut app.modal {
            info.answered(Pending::Leave);
        }
        sent(&mut commands);
        answer(
            &mut app,
            ManageEvent::Gone {
                room_id: INNER.into(),
            },
        );
        assert!(app.modal.is_none());
        assert_eq!(described(&mut commands), ["StopInfo"]);
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("You are no longer in that space")
        );
    }

    #[test]
    fn a_room_goes_into_a_space_and_out_again_from_its_own_side() {
        let (mut app, mut terminal, mut commands) = connected();
        // The room's menu offers the spaces it could go into: only those we may
        // change, and not one that has it already.
        app.set_focus(Pane::Sidebar);
        app.sidebar.cursor = app
            .visible_rooms()
            .iter()
            .position(|index| app.rooms[*index].id == CHEESE)
            .unwrap();
        app.perform(Action::ContextMenu);
        let labels: Vec<String> = match &app.modal {
            Some(Modal::Menu(menu)) => menu.items.iter().map(|item| item.label.clone()).collect(),
            other => panic!("no menu: {other:?}"),
        };
        assert!(labels.contains(&"Add to a space…".to_owned()), "{labels:?}");
        assert!(!labels.iter().any(|label| label.starts_with("Remove from")));
        let at = labels.iter().position(|l| l == "Add to a space…").unwrap();
        app.run_menu_at(at);
        let titles = match &app.modal {
            Some(Modal::Palette(palette)) => palette
                .entries
                .iter()
                .map(|entry| entry.title.clone())
                .collect::<Vec<_>>(),
            other => panic!("no picker: {other:?}"),
        };
        assert_eq!(titles, ["Work"]);
        app.on_event(key(KeyCode::Enter));
        let request = last_request(&app);
        assert_eq!(
            described(&mut commands),
            [format!(
                "EditSpace(request {request}, {SPACE}, {CHEESE}, Add)"
            )]
        );
        // Nobody waits for it: the answer is a toast.
        assert!(app.modal.is_none());
        answer(
            &mut app,
            ManageEvent::SpaceEdited {
                request,
                space_id: SPACE.into(),
                child_id: CHEESE.into(),
                edit: SpaceEdit::Add,
                result: Ok(()),
            },
        );
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("Added Cheese to Work")
        );

        // Out again: with Work selected, Rats is in it. Asked in the room info.
        app.select_space(Some(0));
        app.set_focus(Pane::Composer);
        app.open_room(app.rooms.iter().position(|r| r.id == RATS).unwrap());
        sent(&mut commands);
        app.perform(Action::Manage(Click::RemoveFromSpace));
        assert_eq!(info(&app).current(), Some(InfoRow::RemoveFromSpace));
        find(&screen(&mut app, &mut terminal), "Remove from space");
        sent(&mut commands);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).confirm, Some(Confirm::RemoveFromSpace));
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        let request = last_request(&app);
        assert_eq!(
            described(&mut commands),
            [format!(
                "EditSpace(request {request}, {SPACE}, {RATS}, Remove)"
            )]
        );
        assert_eq!(info(&app).busy, Some("Removing…"));

        // Where we may not change the space, it says so.
        app.on_event(key(KeyCode::Esc));
        app.on_matrix(MatrixEvent::Spaces(vec![space(
            SPACE,
            "Work",
            &[RATS],
            false,
        )]));
        app.select_space(Some(0));
        app.perform(Action::Manage(Click::RemoveFromSpace));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(
            info(&app).error.as_deref(),
            Some("You are not allowed to take rooms out of Work.")
        );
        assert!(info(&app).confirm.is_none());
    }

    /// What the room's menu and the palette offer for taking Rats out of the space
    /// the list is narrowed to: the menu's label, and whether the palette's entry
    /// can be run.
    fn removal_offered(app: &mut App) -> (Option<String>, Option<bool>) {
        app.set_focus(Pane::Sidebar);
        app.sidebar.cursor = app
            .visible_rooms()
            .iter()
            .position(|index| app.rooms[*index].id == RATS)
            .unwrap();
        app.perform(Action::ContextMenu);
        let menu = match &app.modal {
            Some(Modal::Menu(menu)) => menu
                .items
                .iter()
                .map(|item| item.label.clone())
                .find(|label| label.starts_with("Remove from")),
            other => panic!("no menu: {other:?}"),
        };
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::OpenPalette);
        let palette = match &app.modal {
            Some(Modal::Palette(palette)) => palette
                .entries
                .iter()
                .find(|entry| entry.title.starts_with("Remove Rats from"))
                .map(|entry| entry.enabled),
            other => panic!("no palette: {other:?}"),
        };
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        (menu, palette)
    }

    #[test]
    fn taking_a_room_out_is_only_offered_where_the_space_may_be_changed() {
        let (mut app, mut terminal, _commands) = connected();
        // Rats is in both spaces; only Work may be changed.
        app.on_matrix(MatrixEvent::Spaces(vec![
            space(SPACE, "Work", &[RATS, OUTSIDE], true),
            space(FUN, "Fun", &[RATS], false),
        ]));
        app.select_space(Some(1));
        assert_eq!(removal_offered(&mut app), (None, Some(false)));
        // The room info keeps the row, to say why.
        app.open_info(app.rooms.iter().position(|room| room.id == RATS), None);
        select(&mut app, InfoRow::RemoveFromSpace);
        let text = screen(&mut app, &mut terminal);
        find(&text, "Fun (read-only)");
        find(&text, "Read-only: you are not allowed to change the rooms");
        app.on_event(key(KeyCode::Esc));

        app.select_space(Some(0));
        assert_eq!(
            removal_offered(&mut app),
            (Some("Remove from Work…".to_owned()), Some(true))
        );
    }

    #[test]
    fn the_space_line_and_the_palette_lead_to_the_space() {
        let (mut app, mut terminal, mut commands) = connected();
        app.select_space(Some(0));
        let text = screen(&mut app, &mut terminal);
        let (column, row) = find(&text, "Work");
        app.on_event(InputEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
        let labels: Vec<String> = match &app.modal {
            Some(Modal::Menu(menu)) => menu.items.iter().map(|item| item.label.clone()).collect(),
            other => panic!("no menu: {other:?}"),
        };
        assert_eq!(labels, ["Space info", "Add a room…", "Create a space…"]);
        sent(&mut commands);
        app.run_menu_at(0);
        assert!(info(&app).space);
        assert_eq!(described(&mut commands), [format!("Info({SPACE})")]);
        app.on_event(key(KeyCode::Esc));

        app.perform(Action::OpenPalette);
        let titles: Vec<(String, bool)> = match &app.modal {
            Some(Modal::Palette(palette)) => palette
                .entries
                .iter()
                .map(|entry| (entry.title.clone(), entry.enabled))
                .collect(),
            other => panic!("no palette: {other:?}"),
        };
        for wanted in [
            ("Space info: Work", true),
            ("Add a room to Work…", true),
            ("Create a space…", true),
        ] {
            assert!(
                titles.contains(&(wanted.0.to_owned(), wanted.1)),
                "{wanted:?} in {titles:?}"
            );
        }
    }

    #[test]
    fn the_info_of_a_space_says_space_id_while_its_details_are_on_their_way() {
        let (mut app, mut terminal, _commands) = connected();
        app.select_space(Some(0));
        app.perform(Action::SpaceInfo);
        assert!(info(&app).details.is_none());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Space ID");
        assert!(!text.contains("Room ID"), "{text}");
    }

    #[test]
    fn a_picker_that_finds_nothing_says_what_it_looked_for() {
        let (mut app, mut terminal, _commands) = connected();
        app.select_space(Some(0));
        app.perform(Action::Manage(Click::AddRoomToSpace));
        type_text(&mut app, "zzzz");
        find(&screen(&mut app, &mut terminal), "no matching rooms");
        app.on_event(key(KeyCode::Esc));
        app.select_space(None);
        app.open_room(app.rooms.iter().position(|r| r.id == CHEESE).unwrap());
        app.set_focus(Pane::Composer);
        app.perform(Action::Manage(Click::AddToSpace));
        type_text(&mut app, "zzzz");
        let text = screen(&mut app, &mut terminal);
        find(&text, "no matching spaces");
        assert!(!text.contains("no matching commands"), "{text}");
    }

    #[test]
    fn the_name_of_a_space_is_made_safe_where_a_room_info_shows_it() {
        let (mut app, _terminal, _commands) = connected();
        app.on_matrix(MatrixEvent::Spaces(vec![space(
            SPACE,
            "Wo\u{1b}[31mrk",
            &[RATS],
            true,
        )]));
        app.select_space(Some(0));
        app.open_info(app.rooms.iter().position(|r| r.id == RATS), None);
        assert_eq!(
            info(&app)
                .in_space
                .as_ref()
                .map(|space| space.name.as_str()),
            Some("Wo[31mrk")
        );
    }

    #[test]
    fn someone_is_invited_into_a_space_from_its_info() {
        let (mut app, _terminal, mut commands) = connected();
        with_space_info(&mut app, &mut commands, true);
        select(&mut app, InfoRow::Invite);
        app.on_event(key(KeyCode::Enter));
        match &app.modal {
            Some(Modal::New(popup)) => {
                assert_eq!(popup.stage, Stage::Invite);
                assert_eq!(popup.room, Some((SPACE.to_owned(), "Work".to_owned())));
            }
            other => panic!("no invitation form: {other:?}"),
        }
        // The room list knows no spaces; that does not close the form.
        app.on_matrix(MatrixEvent::Rooms(vec![room(RATS, "Rats")]));
        assert!(matches!(app.modal, Some(Modal::New(_))));
        sent(&mut commands);
        type_text(&mut app, "@bob:example.org");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(described(&mut commands), [format!("Invite({SPACE})")]);
    }

    #[test]
    fn the_demo_has_a_space_to_look_at_and_change() {
        let mut app = App::demo();
        app.add_demo_space();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.select_space(Some(0));
        app.perform(Action::SpaceInfo);
        let names: Vec<(String, bool, bool)> = info(&app)
            .children()
            .iter()
            .map(|child| (child.name.clone(), child.joined, child.suggested))
            .collect();
        assert_eq!(
            names,
            [
                ("rust".to_owned(), true, false),
                ("matrix-rat".to_owned(), true, true),
                ("nixos".to_owned(), true, false),
                ("burrow".to_owned(), false, false),
            ]
        );
        select(&mut app, InfoRow::AddChild);
        insta::assert_snapshot!("space_info_demo", screen(&mut app, &mut terminal));

        // Taken out, on the second Del: the list narrowed to the space follows.
        select(&mut app, InfoRow::Child(2));
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(
            info(&app).notice.as_deref(),
            Some("Took nixos out of Rat club")
        );
        assert_eq!(info(&app).children().len(), 3);
        assert!(!app.spaces[0].children.contains(&"nixos".to_owned()));
        // Suggested, and put in again by the picker.
        select(&mut app, InfoRow::Child(0));
        app.on_event(key(KeyCode::Char(' ')));
        assert!(info(&app).children()[0].suggested);
        select(&mut app, InfoRow::AddChild);
        app.on_event(key(KeyCode::Enter));
        insta::assert_snapshot!("space_room_picker", screen(&mut app, &mut terminal));
        type_text(&mut app, "nixos");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).children().len(), 4);
        assert_eq!(
            info(&app).notice.as_deref(),
            Some("Added nixos to Rat club")
        );
        // What would need a server still says so.
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Manage(Click::Stage(Stage::CreateSpace)));
        type_text(&mut app, "New");
        app.perform(Action::Manage(Click::Submit));
        match &app.modal {
            Some(Modal::New(popup)) => assert_eq!(popup.error.as_deref(), Some(NO_ACCOUNT)),
            other => panic!("the new popup is not open: {other:?}"),
        }
    }
}
