//! Room management on `App`: joining, direct chats, creating, inviting, leaving, and
//! the room info view. The popups' state lives in `crate::manage`.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::action::{Action, Pane};
use crate::lineedit::{EditKeys, LineEdit};
use crate::manage::{
    Address, Click, Confirm, FIELD_MAX, InfoPopup, InfoRow, NewPopup, Pending, Stage, clean_input,
    parse_address, parse_alias_local, parse_user,
};
use crate::matrix::{
    Change, ManageCmd, ManageEvent, MatrixCmd, MemberRole, NotifyLevel, RoomDetails, RoomMember,
};
use crate::modal::Modal;
use crate::model::{MsgKind, RoomKind};

/// A room that was joined or created shows up in the room list a moment later. Longer
/// than this and opening it by itself would come as a surprise.
const OPEN_WITHIN: Duration = Duration::from_secs(60);
const NO_ACCOUNT: &str = "The demo has no account to do this with.";
/// What servers accept for a room name.
const NAME_MAX: usize = 255;

#[derive(Debug, Default)]
pub struct ManageState {
    /// The room to open as soon as the room list has it, and since when.
    pending_open: Option<(String, Instant)>,
    /// Numbers the requests whose answers have to find the popup that asked.
    requests: u64,
}

impl App {
    /// The room that room-level commands are about: the one under the cursor while
    /// the room list has the focus, otherwise the open one. The command palette
    /// describes its entries by the same rule.
    fn target_room(&self) -> Option<usize> {
        if self.focus == Pane::Sidebar {
            self.cursor_room()
        } else {
            (self.active < self.rooms.len()).then_some(self.active)
        }
    }

    /// Name of the room the room-level commands would act on now, and whether it is
    /// an invitation.
    pub(super) fn manage_target(&self) -> Option<(String, bool)> {
        let room = self.rooms.get(self.target_room()?)?;
        Some((room.name.clone(), room.kind == RoomKind::Invite))
    }

    /// Sends a command to the worker; false in the demo, which has none.
    fn manage(&mut self, command: ManageCmd) -> bool {
        if self.matrix.is_none() {
            return false;
        }
        self.send_matrix(MatrixCmd::Manage(command));
        true
    }

    fn next_request(&mut self) -> u64 {
        self.manage.requests += 1;
        self.manage.requests
    }

    pub(super) fn perform_manage(&mut self, action: Action) {
        match action {
            Action::NewRoom => self.open_new(Stage::Choose),
            Action::InviteUser => self.open_new(Stage::Invite),
            Action::RoomInfo => self.open_info(self.target_room(), None),
            Action::LeaveRoom => self.open_info(self.target_room(), Some(InfoRow::Leave)),
            Action::Manage(click) => self.manage_click(click),
            _ => {}
        }
    }

    // ----- new chat or room ----------------------------------------------------------

    fn open_new(&mut self, stage: Stage) {
        self.open_new_with(stage, |_| {});
    }

    pub(super) fn open_new_user(&mut self, user_id: &str) {
        self.open_new_with(Stage::Direct, |popup| {
            popup.user.set(user_id);
        });
    }

    pub(super) fn open_new_join(&mut self, address: &str) {
        self.open_new_with(Stage::Join, |popup| {
            popup.address.set(address);
        });
    }

    fn open_new_with(&mut self, stage: Stage, fill: impl FnOnce(&mut NewPopup)) {
        // From the room info view the form takes its place; any other popup stays.
        let from_info = match &self.modal {
            Some(Modal::RoomInfo(info)) => Some(info.room_id.clone()),
            Some(Modal::New(_)) | None => None,
            Some(_) => return,
        };
        if self.images.viewer_open() {
            return;
        }
        if matches!(self.modal, Some(Modal::New(ref popup)) if popup.busy.is_none()) {
            self.cancel_user_search();
        }
        if let Some(Modal::New(popup)) = &mut self.modal {
            if popup.busy.is_none() {
                popup.go(stage);
                fill(popup);
            }
            return;
        }
        let mut popup = NewPopup::new(stage);
        fill(&mut popup);
        if stage == Stage::Invite {
            let room = match &from_info {
                Some(id) => self.rooms.iter().find(|room| room.id == *id),
                None => self.target_room().and_then(|index| self.rooms.get(index)),
            };
            let Some(room) = room else {
                self.show_toast("No room to invite to");
                return;
            };
            if room.kind == RoomKind::Invite {
                self.show_toast("Accept the invitation first");
                return;
            }
            popup.room = Some((room.id.clone(), room.name.clone()));
        }
        if from_info.is_some() {
            self.close_manage();
        }
        self.open_modal(Modal::New(popup));
    }

    fn submit_new(&mut self) {
        let own = self.user_id.clone();
        let request = self.next_request();
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        if popup.busy.is_some() {
            return;
        }
        popup.error = None;
        let not_yourself = |user: &str| match &own {
            Some(own) if own == user => Err("That is your own user ID.".to_owned()),
            _ => Ok(()),
        };
        // Typed into the form on purpose: a chat that does not exist yet is made.
        let chat = |user_id: String| ManageCmd::DirectChat {
            request,
            user_id,
            create: true,
        };
        let prepared: Result<(ManageCmd, &'static str), String> = match popup.stage {
            Stage::Choose => return,
            Stage::Join => parse_address(&popup.address).and_then(|address| match address {
                Address::Room { target, via }
                | Address::Event {
                    room: target, via, ..
                } => Ok((
                    ManageCmd::Join {
                        request,
                        target,
                        via,
                    },
                    "Joining…",
                )),
                // A link to a person: a chat with them is what can be opened.
                Address::User(user_id) => {
                    not_yourself(&user_id)?;
                    Ok((chat(user_id), "Opening the chat…"))
                }
            }),
            Stage::Direct => parse_user(&popup.user).and_then(|user_id| {
                not_yourself(&user_id)?;
                Ok((chat(user_id), "Opening the chat…"))
            }),
            Stage::Invite => parse_user(&popup.user).and_then(|user_id| {
                not_yourself(&user_id)?;
                let room_id = popup.room.as_ref().map(|(id, _)| id.clone());
                let room_id = room_id.ok_or_else(|| "No room to invite to.".to_owned())?;
                Ok((ManageCmd::Invite { room_id, user_id }, "Inviting…"))
            }),
            Stage::Create => (|| {
                // Nothing that is not text goes to the server as a name or topic.
                let name = clean_input(popup.name.trim());
                if name.is_empty() {
                    return Err("Give the room a name.".to_owned());
                }
                if name.len() > NAME_MAX {
                    return Err("That name is too long.".to_owned());
                }
                let alias = if popup.public {
                    Some(parse_alias_local(&popup.alias)?)
                } else {
                    None
                };
                let topic = clean_input(popup.topic.trim());
                Ok((
                    ManageCmd::Create {
                        request,
                        name,
                        topic: (!topic.is_empty()).then_some(topic),
                        encrypted: alias.is_none() && popup.encrypted,
                        alias,
                    },
                    "Creating the room…",
                ))
            })(),
        };
        match prepared {
            Ok((command, busy)) => {
                self.cancel_user_search();
                if let Some(Modal::New(popup)) = &mut self.modal {
                    popup.busy = Some(busy);
                    popup.request = Some(request);
                }
                if !self.manage(command)
                    && let Some(Modal::New(popup)) = &mut self.modal
                {
                    popup.busy = None;
                    popup.request = None;
                    popup.error = Some(NO_ACCOUNT.to_owned());
                }
            }
            Err(error) => popup.error = Some(error),
        }
    }

    fn on_new_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        if code == KeyCode::Esc {
            return self.manage_escape();
        }
        if popup.busy.is_some() {
            return;
        }
        if popup.stage == Stage::Choose {
            match code {
                KeyCode::Up | KeyCode::BackTab => popup.step(-1),
                KeyCode::Down | KeyCode::Tab => popup.step(1),
                KeyCode::Enter | KeyCode::Right => popup.go(Stage::CHOICES[popup.choice]),
                KeyCode::Char(c @ '1'..='3') if plain => {
                    popup.choice = c as usize - '1' as usize;
                    popup.go(Stage::CHOICES[popup.choice]);
                }
                _ => {}
            }
            return;
        }
        let search_stage = matches!(popup.stage, Stage::Direct | Stage::Invite);
        if search_stage && !popup.user_results.is_empty() {
            match code {
                KeyCode::Down => {
                    popup.step_user_search(1);
                    return;
                }
                KeyCode::Up => {
                    popup.step_user_search(-1);
                    return;
                }
                KeyCode::Enter if popup.user_selected.is_some() => {
                    if let Some(selected) = popup.user_selected {
                        self.pick_user_search(selected);
                    }
                    return;
                }
                _ => {}
            }
        }
        match code {
            KeyCode::Tab | KeyCode::Down => popup.step(1),
            KeyCode::BackTab | KeyCode::Up => popup.step(-1),
            KeyCode::Enter if popup.focus + 1 < popup.slots() => popup.step(1),
            KeyCode::Enter => self.submit_new(),
            KeyCode::Left | KeyCode::Right if popup.field_mut().is_none() && plain => {
                popup.flip(popup.focus);
            }
            KeyCode::Char(' ') if popup.field_mut().is_none() && plain => {
                popup.flip(popup.focus);
            }
            _ => {
                if let Some(field) = popup.field_mut()
                    && field.handle_key(code, mods, EditKeys::FULL).changed()
                {
                    popup.error = None;
                    if search_stage {
                        self.on_user_search_input_changed();
                    }
                }
            }
        }
    }

    // ----- room info -----------------------------------------------------------------

    /// Shows the room info for the room at `index`, with `select` selected.
    pub(super) fn open_info(&mut self, index: Option<usize>, select: Option<InfoRow>) {
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        let Some(room) = index.and_then(|index| self.rooms.get(index)) else {
            self.show_toast("No room is open");
            return;
        };
        let mut popup = InfoPopup::new(&room.id, &room.name, room.kind);
        if self.matrix.is_none() {
            popup.details = Some(demo_details(room));
        }
        if let Some(row) = select {
            popup.select(row);
        }
        let room_id = room.id.clone();
        self.open_modal(Modal::RoomInfo(popup));
        self.manage(ManageCmd::Info { room_id });
    }

    /// Closes either room popup. What is under way on the server carries on.
    pub(super) fn close_manage(&mut self) {
        if matches!(self.modal, Some(Modal::New(_))) {
            self.cancel_user_search();
        }
        if matches!(self.modal, Some(Modal::RoomInfo(_))) {
            self.manage(ManageCmd::StopInfo);
        }
        if matches!(self.modal, Some(Modal::New(_) | Modal::RoomInfo(_))) {
            self.modal = None;
        }
    }

    /// Esc: one step back.
    pub(super) fn manage_escape(&mut self) {
        let back_to_choose = matches!(
            &self.modal,
            Some(Modal::New(popup))
                if popup.busy.is_none() && !popup.single && popup.stage != Stage::Choose
        );
        if back_to_choose {
            self.cancel_user_search();
            if let Some(Modal::New(popup)) = &mut self.modal {
                popup.go(Stage::Choose);
            }
            return;
        }
        match &mut self.modal {
            Some(Modal::New(_)) => self.close_manage(),
            Some(Modal::RoomInfo(info)) => {
                if info.editing.take().is_some() || info.confirm.take().is_some() {
                    // Stepped back from typing or from the question.
                } else if !info.filter.is_empty() {
                    info.set_filter(String::new());
                } else {
                    self.close_manage();
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_manage_paste(&mut self, first_line: &str) {
        match &mut self.modal {
            Some(Modal::New(popup)) if popup.busy.is_none() => {
                let search_stage = matches!(popup.stage, Stage::Direct | Stage::Invite);
                if let Some(field) = popup.field_mut()
                    && field.paste(first_line).changed()
                {
                    popup.error = None;
                    if search_stage {
                        self.on_user_search_input_changed();
                    }
                }
            }
            Some(Modal::RoomInfo(info)) => match &mut info.editing {
                Some((_, edit)) => {
                    edit.paste(first_line);
                }
                None => {
                    info.apply_filter(|f| f.paste(first_line));
                }
            },
            _ => {}
        }
    }

    /// The mouse wheel over the room info view.
    pub(super) fn manage_scroll(&mut self, lines: isize) {
        if let Some(Modal::RoomInfo(info)) = &mut self.modal {
            info.scroll_by(lines);
        }
    }

    pub(super) fn on_manage_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if matches!(self.modal, Some(Modal::New(_))) {
            return self.on_new_key(code, mods);
        }
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if code == KeyCode::Esc {
            return self.manage_escape();
        }
        if let Some((_, edit)) = &mut info.editing {
            if code == KeyCode::Enter {
                self.submit_edit();
            } else {
                edit.handle_key(code, mods, EditKeys::FULL);
            }
            return;
        }
        let page = info.page.saturating_sub(1).max(1) as isize;
        let row = info.current();
        match code {
            KeyCode::F(4) if mods.is_empty() => self.close_manage(),
            KeyCode::Up | KeyCode::BackTab => info.step(-1),
            KeyCode::Down | KeyCode::Tab => info.step(1),
            KeyCode::PageUp => info.step(-page),
            KeyCode::PageDown => info.step(page),
            KeyCode::Home => {
                // All the way: the first row, and what stands above it.
                info.step(-(isize::MAX / 2));
                info.scroll = 0;
                info.reveal = false;
            }
            KeyCode::End => info.step(isize::MAX / 2),
            KeyCode::Left | KeyCode::Right if row == Some(InfoRow::Notify) && plain => {
                self.info_activate(InfoRow::Notify, if code == KeyCode::Left { -1 } else { 1 });
            }
            KeyCode::Enter => {
                if let Some(row) = row {
                    self.info_activate(row, 0);
                }
            }
            KeyCode::Delete if matches!(row, Some(InfoRow::Member(_))) => self.remove_member(),
            KeyCode::Char('i')
                if mods == KeyModifiers::ALT && matches!(row, Some(InfoRow::Member(_))) =>
            {
                self.toggle_ignore_member();
            }
            _ => {
                let keys = EditKeys {
                    arrows: row != Some(InfoRow::Notify),
                    home_end: false,
                    delete: !matches!(row, Some(InfoRow::Member(_))),
                    kill_to_end: true,
                };
                info.apply_filter(|f| f.handle_key(code, mods, keys));
            }
        }
    }

    fn manage_click(&mut self, click: Click) {
        match click {
            Click::Stage(stage) => self.open_new(stage),
            Click::Slot(slot) => {
                if let Some(Modal::New(popup)) = &mut self.modal
                    && popup.busy.is_none()
                    && slot < popup.slots()
                {
                    if popup.stage == Stage::Choose {
                        popup.choice = slot;
                    } else {
                        popup.focus = slot;
                    }
                }
            }
            Click::Flip(slot) => {
                if let Some(Modal::New(popup)) = &mut self.modal
                    && popup.busy.is_none()
                    && slot < popup.slots()
                {
                    popup.focus = slot;
                    popup.flip(slot);
                }
            }
            Click::Submit => {
                if let Some(Modal::New(popup)) = &mut self.modal
                    && popup.busy.is_none()
                {
                    popup.focus = popup.slots() - 1;
                    self.submit_new();
                }
            }
            Click::Row(index) | Click::Step(index, _) | Click::Select(index) => {
                let Some(Modal::RoomInfo(info)) = &mut self.modal else {
                    return;
                };
                let Some(row) = info.rows().get(index).copied() else {
                    return;
                };
                if info.editing.is_some() {
                    return;
                }
                if info.selected != index {
                    info.selected = index;
                    info.confirm = None;
                }
                match click {
                    Click::Row(_) => self.info_activate(row, 0),
                    Click::Step(_, step) => self.info_activate(row, step),
                    _ => {}
                }
            }
            Click::MemberChat => {
                if let Some(Modal::RoomInfo(info)) = &self.modal
                    && let Some(row @ InfoRow::Member(_)) = info.current()
                {
                    self.info_activate(row, 0);
                }
            }
            Click::MemberRemove => self.remove_member(),
            Click::MemberIgnore => self.toggle_ignore_member(),
            Click::Favourite => {
                let Some(room) = self.target_room().and_then(|index| self.rooms.get(index)) else {
                    self.show_toast("No room is open");
                    return;
                };
                if room.kind == RoomKind::Invite {
                    self.show_toast("Accept the invitation first");
                    return;
                }
                let on = room.kind != RoomKind::Favourite;
                let room_id = room.id.clone();
                if !self.manage(ManageCmd::SetFavourite { room_id, on }) {
                    self.show_toast(NO_ACCOUNT);
                }
            }
            // The name above the messages is the open room's, wherever the focus is.
            Click::OpenInfo => {
                let open = (self.active < self.rooms.len()).then_some(self.active);
                self.open_info(open, None);
            }
        }
    }

    /// Does what the row is for. `step` is the direction for rows with a value to
    /// change; 0 for Enter or a click.
    fn info_activate(&mut self, row: InfoRow, step: i8) {
        let own = self.user_id.clone();
        let connected = self.matrix.is_some();
        let request = self.next_request();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() {
            return;
        }
        info.error = None;
        let room_id = info.room_id.clone();
        let confirmed = std::mem::take(&mut info.confirm);
        // Nothing is changed, shown as changed or asked about where there is no
        // account to do it with.
        let sends = !matches!(row, InfoRow::Name | InfoRow::Topic | InfoRow::Invite);
        if sends && !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        let (command, pending, busy) = match row {
            InfoRow::Favourite => {
                let on = !info
                    .details
                    .as_ref()
                    .map_or(info.kind == RoomKind::Favourite, |details| {
                        details.favourite
                    });
                // Shown at once; the server's answer corrects it if need be.
                if let Some(details) = &mut info.details {
                    details.favourite = on;
                }
                (
                    ManageCmd::SetFavourite { room_id, on },
                    Pending::Change(Change::Favourite(on)),
                    "Saving…",
                )
            }
            InfoRow::Notify => {
                let Some(details) = &mut info.details else {
                    return;
                };
                let levels = NotifyLevel::ALL;
                let at = levels
                    .iter()
                    .position(|level| Some(*level) == details.notify)
                    .unwrap_or(0) as isize;
                let step = if step < 0 { -1 } else { 1 };
                let level = levels[(at + step).rem_euclid(levels.len() as isize) as usize];
                details.notify = Some(level);
                (
                    ManageCmd::SetNotify { room_id, level },
                    Pending::Change(Change::Notify),
                    "Saving…",
                )
            }
            InfoRow::Name | InfoRow::Topic => {
                let current = info.details.as_ref().map_or(String::new(), |details| {
                    if row == InfoRow::Name {
                        details.name.clone()
                    } else {
                        crate::text::sanitize_line(&details.topic)
                    }
                });
                info.editing = Some((row, LineEdit::with_text(current).with_max_bytes(FIELD_MAX)));
                return;
            }
            InfoRow::Invite => return self.open_new(Stage::Invite),
            InfoRow::Leave => {
                if confirmed != Some(Confirm::Leave) {
                    info.confirm = Some(Confirm::Leave);
                    return;
                }
                (ManageCmd::Leave { room_id }, Pending::Leave, "Leaving…")
            }
            InfoRow::Member(_) => {
                let Some(member) = info.member(row) else {
                    return;
                };
                if member.own || own.as_deref() == Some(&member.user_id) {
                    info.error = Some("That is you.".to_owned());
                    return;
                }
                let user_id = member.user_id.clone();
                // The first Enter only opens a chat that exists. Making one (a room,
                // and an invitation to that person) takes a second, after the question.
                let create = confirmed == Some(Confirm::NewChat(user_id.clone()));
                (
                    ManageCmd::DirectChat {
                        request,
                        user_id,
                        create,
                    },
                    Pending::Chat(request),
                    if create {
                        "Starting the chat…"
                    } else {
                        "Opening the chat…"
                    },
                )
            }
        };
        info.wait_for(pending, busy);
        self.manage(command);
    }

    /// Del on a member: asks first, removes on the second press.
    fn remove_member(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        let Some(member) = info.selected_member().cloned() else {
            return;
        };
        if !member.removable {
            info.confirm = None;
            info.error = Some(if member.own {
                "To go yourself, use “Leave”.".to_owned()
            } else {
                "You are not allowed to remove this member.".to_owned()
            });
            return;
        }
        let user_id = member.user_id;
        if info.confirm != Some(Confirm::Remove(user_id.clone())) {
            info.confirm = Some(Confirm::Remove(user_id));
            return;
        }
        info.confirm = None;
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        info.wait_for(Pending::Change(Change::Kick), "Removing…");
        let room_id = info.room_id.clone();
        self.manage(ManageCmd::Kick { room_id, user_id });
    }

    fn toggle_ignore_member(&mut self) {
        let connected = self.matrix.is_some();
        let already = {
            let Some(Modal::RoomInfo(info)) = &self.modal else {
                return;
            };
            info.selected_member()
                .is_some_and(|member| self.ignored_users.contains(&member.user_id))
        };
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        let Some(member) = info.selected_member().cloned() else {
            return;
        };
        if member.own {
            info.confirm = None;
            info.error = Some("You cannot ignore your own account.".to_owned());
            return;
        }
        let user_id = member.user_id;
        let expected = if already {
            Confirm::Unignore(user_id.clone())
        } else {
            Confirm::Ignore(user_id.clone())
        };
        if info.confirm.as_ref() != Some(&expected) {
            info.confirm = Some(expected);
            return;
        }
        info.confirm = None;
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        self.send_ignore_user(user_id, !already);
    }

    /// Enter while the name or topic is being typed.
    fn submit_edit(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        let Some((row, text)) = &info.editing else {
            return;
        };
        let row = *row;
        let text = clean_input(text.trim());
        let room_id = info.room_id.clone();
        let unchanged = info.details.as_ref().is_some_and(|details| match row {
            InfoRow::Name => details.name == text,
            _ => crate::text::sanitize_line(&details.topic) == text,
        });
        if unchanged {
            info.editing = None;
            return;
        }
        let (command, change) = match row {
            InfoRow::Name if text.is_empty() => {
                info.error = Some("A room needs a name.".to_owned());
                return;
            }
            InfoRow::Name if text.len() > NAME_MAX => {
                info.error = Some("That name is too long.".to_owned());
                return;
            }
            InfoRow::Name => (
                ManageCmd::SetName {
                    room_id,
                    name: text,
                },
                Change::Name,
            ),
            _ => (
                ManageCmd::SetTopic {
                    room_id,
                    topic: text,
                },
                Change::Topic,
            ),
        };
        info.editing = None;
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        info.error = None;
        info.wait_for(Pending::Change(change), "Saving…");
        self.manage(command);
    }

    // ----- answers from the worker ---------------------------------------------------

    pub(super) fn on_manage(&mut self, event: ManageEvent) {
        match event {
            ManageEvent::Opened { request, result } => {
                // Only the popup that asked for exactly this is touched by the answer.
                let asked = match &mut self.modal {
                    Some(Modal::New(popup)) if popup.request == Some(request) => {
                        popup.request = None;
                        popup.busy = None;
                        true
                    }
                    Some(Modal::RoomInfo(info)) => info.answered(Pending::Chat(request)),
                    _ => false,
                };
                match (result, asked) {
                    (Ok(room_id), true) => {
                        self.close_manage();
                        self.manage.pending_open = Some((room_id, Instant::now()));
                        self.open_pending_room();
                    }
                    // Whoever asked has closed the form and moved on: the room is
                    // there, but nothing jumps to it or closes what is open now.
                    (Ok(_), false) => self.show_toast("Done: the room is in your room list"),
                    (Err(error), true) => match &mut self.modal {
                        Some(Modal::New(popup)) => popup.error = Some(error),
                        Some(Modal::RoomInfo(info)) => info.error = Some(error),
                        _ => {}
                    },
                    (Err(error), false) => self.show_toast(error),
                }
            }
            ManageEvent::NoDirectChat { request, user_id } => {
                if let Some(Modal::RoomInfo(info)) = &mut self.modal
                    && info.answered(Pending::Chat(request))
                {
                    // The question belongs to that person's row, wherever the
                    // selection went meanwhile. Gone from the list: nothing to ask.
                    let row = info.rows().into_iter().find(
                        |row| matches!(info.member(*row), Some(member) if member.user_id == user_id),
                    );
                    if let Some(row) = row {
                        info.select(row);
                        info.confirm = Some(Confirm::NewChat(user_id));
                    }
                }
            }
            ManageEvent::Left { room_id, result } => {
                let shown = match &mut self.modal {
                    Some(Modal::RoomInfo(info)) if info.room_id == room_id => {
                        info.answered(Pending::Leave);
                        true
                    }
                    _ => false,
                };
                match result {
                    Ok(()) => {
                        // The room list may have dropped the room already.
                        let name = match &self.modal {
                            Some(Modal::RoomInfo(info)) if shown => Some(info.name.clone()),
                            _ => self
                                .rooms
                                .iter()
                                .find(|room| room.id == room_id)
                                .map(|room| room.name.clone()),
                        };
                        if shown {
                            self.close_manage();
                        }
                        self.show_toast(match name {
                            Some(name) => format!("Left {name}"),
                            None => "Left the room".to_owned(),
                        });
                    }
                    Err(error) => match &mut self.modal {
                        Some(Modal::RoomInfo(info)) if shown => info.error = Some(error),
                        _ => self.show_toast(error),
                    },
                }
            }
            ManageEvent::Done {
                room_id,
                change,
                result,
            } => self.on_change_done(&room_id, change, result),
            ManageEvent::Info { room_id, result } => {
                if let Some(Modal::RoomInfo(info)) = &mut self.modal
                    && info.room_id == room_id
                {
                    match result {
                        Ok(details) => info.set_details(*details),
                        // What is shown stays, with the reason it may be out of date.
                        Err(error) => info.error = Some(error),
                    }
                }
            }
        }
    }

    fn on_change_done(&mut self, room_id: &str, change: Change, result: Result<(), String>) {
        let done = match change {
            Change::Invite => "Invitation sent",
            Change::Kick => "Removed from the room",
            Change::Name => "Room renamed",
            Change::Topic => "Topic changed",
            Change::Favourite(true) => "Added to favourites",
            Change::Favourite(false) => "Removed from favourites",
            Change::Notify => "",
        };
        match &mut self.modal {
            Some(Modal::New(popup))
                if change == Change::Invite
                    && popup.stage == Stage::Invite
                    && popup.busy.is_some()
                    && popup.room.as_ref().is_some_and(|(id, _)| id == room_id) =>
            {
                match result {
                    Ok(()) => {
                        self.modal = None;
                        self.show_toast(done);
                    }
                    Err(error) => {
                        popup.busy = None;
                        popup.error = Some(error);
                    }
                }
            }
            // The change this view is waiting for; any other answer is not its own.
            Some(Modal::RoomInfo(info))
                if info.room_id == room_id && info.pending == Some(Pending::Change(change)) =>
            {
                info.answered(Pending::Change(change));
                match result {
                    Ok(()) => {
                        if !done.is_empty() {
                            self.show_toast(done);
                        }
                    }
                    Err(error) => {
                        info.error = Some(error);
                        // What was shown ahead of the answer is not true; look again.
                        let room_id = room_id.to_owned();
                        self.manage(ManageCmd::Info { room_id });
                    }
                }
            }
            _ => match result {
                Ok(()) if !done.is_empty() => self.show_toast(done),
                Ok(()) => {}
                Err(error) => self.show_toast(error),
            },
        }
    }

    /// Keeps the room popups in step with the room list: an accepted invitation shows
    /// what a room offers, a room that is gone takes its popup with it.
    fn follow_room_list(&mut self) {
        let (room_id, leaving) = match &self.modal {
            Some(Modal::RoomInfo(info)) => {
                (info.room_id.clone(), info.pending == Some(Pending::Leave))
            }
            Some(Modal::New(popup)) if popup.stage == Stage::Invite => match &popup.room {
                Some((id, _)) => (id.clone(), false),
                None => return,
            },
            _ => return,
        };
        let listed = self
            .rooms
            .iter()
            .find(|room| room.id == room_id)
            .map(|room| (room.kind, room.name.clone()));
        match (listed, &mut self.modal) {
            (Some((kind, name)), Some(Modal::RoomInfo(info))) => {
                if info.details.is_none() {
                    info.name = name;
                }
                info.set_kind(kind);
            }
            (Some(_), _) => {}
            // Our own leaving closes the view when its answer is there.
            (None, _) if leaving => {}
            (None, _) => {
                self.close_manage();
                self.show_toast("You are no longer in that room");
            }
        }
    }

    /// Opens the room that was just joined or created, once the room list has it.
    /// Called when the answer arrives and after every update of the list.
    pub(super) fn open_pending_room(&mut self) {
        self.follow_room_list();
        let Some((room_id, since)) = &self.manage.pending_open else {
            return;
        };
        if since.elapsed() > OPEN_WITHIN {
            self.manage.pending_open = None;
            return;
        }
        let Some(index) = self.rooms.iter().position(|room| room.id == *room_id) else {
            return;
        };
        self.manage.pending_open = None;
        if index != self.active || self.thread_open() {
            self.open_room(index);
        }
        if let Some(at) = self.visible_rooms().iter().position(|i| *i == index) {
            self.sidebar.cursor = at;
            self.sidebar.reveal_cursor = true;
        }
    }
}

/// The demo's rooms have no server to ask: everyone who wrote is a member.
fn demo_details(room: &crate::model::Room) -> RoomDetails {
    let mut members: Vec<RoomMember> = Vec::new();
    for message in &room.messages {
        if matches!(message.kind, MsgKind::System | MsgKind::ReadMarker)
            || message.sender.is_empty()
            || members.iter().any(|member| member.name == message.sender)
        {
            continue;
        }
        members.push(RoomMember {
            user_id: format!("@{}:example.org", message.sender),
            name: message.sender.clone(),
            role: if members.is_empty() {
                MemberRole::Admin
            } else {
                MemberRole::User
            },
            invited: false,
            own: message.own,
            removable: false,
        });
    }
    RoomDetails {
        name: room.name.clone(),
        topic: "Demo data. Log in to see your own rooms.".to_owned(),
        alias: (room.kind != RoomKind::Person).then(|| format!("#{}:example.org", room.name)),
        encrypted: Some(room.kind == RoomKind::Person),
        direct: room.kind == RoomKind::Person,
        joined: members.len() as u64,
        invited: 0,
        favourite: room.kind == RoomKind::Favourite,
        notify: Some(NotifyLevel::All),
        can_rename: false,
        can_set_topic: false,
        can_invite: false,
        members,
        ..RoomDetails::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ui;
    use crate::matrix::{MatrixEvent, MatrixHandle, RoomCategory, RoomSummary};
    use crossterm::event::{Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    const ROOM: &str = "!rats:example.org";
    const CHAT: &str = "!bob:example.org";

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
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

    fn summary(id: &str, name: &str, category: RoomCategory) -> RoomSummary {
        RoomSummary {
            id: id.into(),
            name: name.into(),
            category,
            notifications: 0,
            server_notifications: 0,
            mention: false,
            unread: false,
            preview: None,
            avatar: None,
            alias: None,
        }
    }

    /// An app with an account, two rooms, and the commands it sends.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
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

    /// The answer to the request made last.
    fn answer_opened(app: &mut App, result: Result<String, String>) {
        let request = app.manage.requests;
        answer(app, ManageEvent::Opened { request, result });
    }

    fn answer(app: &mut App, event: ManageEvent) {
        app.on_matrix(MatrixEvent::Manage(event));
    }

    fn member(name: &str, role: MemberRole, invited: bool) -> RoomMember {
        RoomMember {
            user_id: format!("@{name}:example.org"),
            name: name.to_owned(),
            role,
            invited,
            own: name == "me",
            removable: name != "me" && role < MemberRole::Admin,
        }
    }

    fn details() -> RoomDetails {
        RoomDetails {
            name: "Rats".into(),
            topic: "All about rats, and a topic long enough that it has to wrap onto a second \
                    line of the popup to be read in full."
                .into(),
            alias: Some("#rats:example.org".into()),
            encrypted: Some(true),
            direct: false,
            joined: 3,
            invited: 1,
            favourite: false,
            notify: Some(NotifyLevel::All),
            can_rename: true,
            can_set_topic: true,
            can_invite: true,
            members: vec![
                member("me", MemberRole::Admin, false),
                member("carol", MemberRole::Moderator, false),
                member("bob", MemberRole::User, false),
                member("dave", MemberRole::User, true),
            ],
            ..RoomDetails::default()
        }
    }

    fn info(app: &App) -> &InfoPopup {
        match &app.modal {
            Some(Modal::RoomInfo(info)) => info,
            other => panic!("room info is not open: {other:?}"),
        }
    }

    fn new_popup(app: &App) -> &NewPopup {
        match &app.modal {
            Some(Modal::New(popup)) => popup,
            other => panic!("the new popup is not open: {other:?}"),
        }
    }

    /// Opens room info for the open room and delivers its details.
    fn with_info(app: &mut App, commands: &mut UnboundedReceiver<MatrixCmd>) {
        app.on_event(key(KeyCode::F(4)));
        assert!(matches!(
            sent(commands).as_slice(),
            [ManageCmd::Info { room_id }] if room_id == ROOM
        ));
        answer(
            app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(details())),
            },
        );
    }

    #[test]
    fn chooser_by_keys_and_back() {
        let (mut app, mut terminal, _commands) = connected();
        app.on_event(ctrl('n'));
        assert_eq!(new_popup(&app).stage, Stage::Choose);
        let text = screen(&mut app, &mut terminal);
        for needle in ["Join a room", "Start a direct chat", "Create a room"] {
            find(&text, needle);
        }
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(new_popup(&app).stage, Stage::Direct);
        // Esc goes back to the chooser, then closes.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(new_popup(&app).stage, Stage::Choose);
        app.on_event(key(KeyCode::Char('3')));
        assert_eq!(new_popup(&app).stage, Stage::Create);
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn joining_validates_reports_and_opens_the_room() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_event(ctrl('n'));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(new_popup(&app).stage, Stage::Join);
        // Garbage never reaches the server.
        type_text(&mut app, "not an address");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        assert!(new_popup(&app).error.is_some());
        find(&screen(&mut app, &mut terminal), "not a room address");
        // A pasted link, as it is.
        app.on_event(ctrl('u'));
        app.on_event(Event::Paste(
            "https://matrix.to/#/#new:example.org?via=example.org\n".into(),
        ));
        assert!(new_popup(&app).error.is_none());
        app.on_event(key(KeyCode::Enter));
        match sent(&mut commands).as_slice() {
            [ManageCmd::Join { target, via, .. }] => {
                assert_eq!(target, "#new:example.org");
                assert_eq!(via, &["example.org"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(new_popup(&app).busy.is_some());
        find(&screen(&mut app, &mut terminal), "Joining…");
        // Locked while waiting: nothing is sent twice.
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "x");
        assert!(sent(&mut commands).is_empty());
        // The server's refusal is shown in the form, made safe.
        answer_opened(&mut app, Err("Could not join: not found".into()));
        assert!(new_popup(&app).busy.is_none());
        find(
            &screen(&mut app, &mut terminal),
            "Could not join: not found",
        );
        // Again; this time it works. The room is opened when the list has it.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands).len(), 1);
        answer_opened(&mut app, Ok("!new:example.org".into()));
        assert!(app.modal.is_none());
        assert_eq!(app.room().unwrap().id, ROOM);
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary("!new:example.org", "New", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
        ]));
        assert_eq!(app.room().unwrap().id, "!new:example.org");
        assert_eq!(app.focus, Pane::Composer);
        // Only once: a later update of the list changes nothing.
        app.perform(Action::OpenRoom(0));
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary("!new:example.org", "New", RoomCategory::Room),
        ]));
        assert_eq!(app.room().unwrap().id, ROOM);
    }

    #[test]
    fn a_link_to_a_person_in_the_join_form_opens_a_chat() {
        let (mut app, _terminal, mut commands) = connected();
        app.perform(Action::Manage(Click::Stage(Stage::Join)));
        type_text(&mut app, "https://matrix.to/#/@carol:example.org");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::DirectChat { user_id, .. }] if user_id == "@carol:example.org"
        ));
    }

    #[test]
    fn direct_chat_validates_and_reuses_what_is_there() {
        let (mut app, _terminal, mut commands) = connected();
        app.perform(Action::Manage(Click::Stage(Stage::Direct)));
        // Opened at the form: Esc closes instead of showing the chooser.
        assert!(new_popup(&app).single);
        for bad in ["bob", "@bob", "#room:example.org", "@me:example.org"] {
            app.on_event(ctrl('u'));
            type_text(&mut app, bad);
            app.on_event(key(KeyCode::Enter));
            assert!(new_popup(&app).error.is_some(), "{bad}");
            assert!(sent(&mut commands).is_empty(), "{bad}");
        }
        app.on_event(ctrl('u'));
        type_text(&mut app, "bob:example.org");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::DirectChat { user_id, .. }] if user_id == "@bob:example.org"
        ));
        // The worker found the existing chat: it is in the list already and opens.
        answer_opened(&mut app, Ok(CHAT.into()));
        assert!(app.modal.is_none());
        assert_eq!(app.room().unwrap().id, CHAT);
        app.perform(Action::Manage(Click::Stage(Stage::Direct)));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn creating_private_and_public_rooms() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::Manage(Click::Stage(Stage::Create)));
        // No name, no room.
        for _ in 0..NewPopup::BUTTON + 1 {
            app.on_event(key(KeyCode::Enter));
        }
        assert!(sent(&mut commands).is_empty());
        assert_eq!(
            new_popup(&app).error.as_deref(),
            Some("Give the room a name.")
        );
        // Private and encrypted unless said otherwise.
        click_on(&mut app, &mut terminal, "Name");
        type_text(&mut app, " Rat plans ");
        app.on_event(key(KeyCode::Tab));
        type_text(&mut app, "what next");
        click_on(&mut app, &mut terminal, " Create room ");
        match sent(&mut commands).as_slice() {
            [
                ManageCmd::Create {
                    name,
                    topic,
                    alias: None,
                    encrypted: true,
                    ..
                },
            ] => {
                assert_eq!(name, "Rat plans");
                assert_eq!(topic.as_deref(), Some("what next"));
            }
            other => panic!("{other:?}"),
        }
        answer_opened(
            &mut app,
            Err("Could not create the room: not allowed".into()),
        );
        // Public: needs an address, and only its first part.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        assert_eq!(new_popup(&app).focus, NewPopup::ACCESS);
        app.on_event(key(KeyCode::Char(' ')));
        assert!(new_popup(&app).public);
        find(&screen(&mut app, &mut terminal), "Address");
        click_on(&mut app, &mut terminal, " Create room ");
        assert!(sent(&mut commands).is_empty());
        click_on(&mut app, &mut terminal, "Address");
        type_text(&mut app, "#rats:example.org");
        click_on(&mut app, &mut terminal, " Create room ");
        assert!(sent(&mut commands).is_empty());
        assert!(new_popup(&app).error.as_ref().unwrap().contains("colon"));
        click_on(&mut app, &mut terminal, "Address");
        app.on_event(ctrl('u'));
        type_text(&mut app, "rat-plans");
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        match sent(&mut commands).as_slice() {
            [
                ManageCmd::Create {
                    alias: Some(alias),
                    encrypted: false,
                    ..
                },
            ] => assert_eq!(alias, "rat-plans"),
            other => panic!("{other:?}"),
        }
        // Switching a private room's encryption off is possible, by the switch.
        answer_opened(&mut app, Err("no".into()));
        click_on(&mut app, &mut terminal, "Public");
        assert!(!new_popup(&app).public);
        click_on(&mut app, &mut terminal, "Encryption");
        assert!(!new_popup(&app).encrypted);
        click_on(&mut app, &mut terminal, " Create room ");
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Create {
                alias: None,
                encrypted: false,
                ..
            }]
        ));
    }

    /// Clicks on the first place `needle` is drawn at.
    fn click_on(app: &mut App, terminal: &mut Terminal<TestBackend>, needle: &str) {
        let text = screen(app, terminal);
        let (column, row) = find(&text, needle);
        app.on_event(click(column, row));
    }

    #[test]
    fn inviting_from_room_info() {
        let (mut app, mut terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        click_on(&mut app, &mut terminal, "Invite someone");
        let popup = new_popup(&app);
        assert_eq!(popup.stage, Stage::Invite);
        assert_eq!(popup.room.as_ref().unwrap().0, ROOM);
        // The info view no longer needs updates.
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::StopInfo]
        ));
        find(&screen(&mut app, &mut terminal), "Rats");
        type_text(&mut app, "nobody");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        app.on_event(ctrl('u'));
        type_text(&mut app, "@erin:example.org");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Invite { room_id, user_id }]
                if room_id == ROOM && user_id == "@erin:example.org"
        ));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Invite,
                result: Err("Could not invite: not allowed".into()),
            },
        );
        assert_eq!(
            new_popup(&app).error.as_deref(),
            Some("Could not invite: not allowed")
        );
        app.on_event(key(KeyCode::Enter));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Invite,
                result: Ok(()),
            },
        );
        assert!(app.modal.is_none());
        assert_eq!(app.toast.as_ref().unwrap().0, "Invitation sent");
    }

    #[test]
    fn leaving_asks_first() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::LeaveRoom);
        sent(&mut commands);
        // Selected, not armed: one Enter only asks.
        assert_eq!(info(&app).current(), Some(InfoRow::Leave));
        assert_eq!(info(&app).confirm, None);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).confirm, Some(Confirm::Leave));
        find(&screen(&mut app, &mut terminal), "Enter again to leave");
        assert!(sent(&mut commands).is_empty());
        // Esc takes the question back, moving away does too.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(info(&app).confirm, None);
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Down));
        assert_eq!(info(&app).confirm, None);
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Leave { room_id }] if room_id == ROOM
        ));
        // A failure stays on screen; success closes the view.
        answer(
            &mut app,
            ManageEvent::Left {
                room_id: ROOM.into(),
                result: Err("Could not leave: the server could not be reached".into()),
            },
        );
        find(&screen(&mut app, &mut terminal), "could not be reached");
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands).len(), 1);
        answer(
            &mut app,
            ManageEvent::Left {
                room_id: ROOM.into(),
                result: Ok(()),
            },
        );
        assert!(app.modal.is_none());
        assert_eq!(app.toast.as_ref().unwrap().0, "Left Rats");
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::StopInfo]
        ));
        // The list without the room: another one is shown.
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            CHAT,
            "bob",
            RoomCategory::Person,
        )]));
        assert_eq!(app.room().unwrap().id, CHAT);
    }

    #[test]
    fn wording_fits_chats_and_invitations() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
            summary("!inv:example.org", "Party", RoomCategory::Invite),
        ]));
        app.perform(Action::OpenRoom(1));
        app.perform(Action::LeaveRoom);
        find(&screen(&mut app, &mut terminal), "Leave this chat");
        app.on_event(key(KeyCode::Esc));
        // The room under the cursor of the room list, not the open one.
        app.perform(Action::Focus(Pane::Sidebar));
        app.perform(Action::SidebarUp);
        app.perform(Action::SidebarUp);
        app.perform(Action::LeaveRoom);
        assert_eq!(info(&app).room_id, "!inv:example.org");
        assert_eq!(info(&app).rows(), [InfoRow::Leave]);
        let text = screen(&mut app, &mut terminal);
        find(&text, "Decline invitation");
        app.on_event(key(KeyCode::Enter));
        find(&screen(&mut app, &mut terminal), "Enter again to decline");
        app.on_event(key(KeyCode::Enter));
        assert!(
            sent(&mut commands).iter().any(
                |c| matches!(c, ManageCmd::Leave { room_id } if room_id == "!inv:example.org")
            )
        );
        // Nobody is invited to an invitation.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Sidebar);
        app.perform(Action::InviteUser);
        assert!(app.modal.is_none());
    }

    #[test]
    fn members_filter_chat_and_removal() {
        let (mut app, mut terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        let text = screen(&mut app, &mut terminal);
        for needle in ["@carol:example.org", "moderator", "admin", "invited"] {
            find(&text, needle);
        }
        // Typing narrows the list and selects the first match.
        type_text(&mut app, "bo");
        assert_eq!(info(&app).selected_member().unwrap().name, "bob");
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("@carol:example.org"));
        // Removing needs a second Del, and the right to do it.
        app.on_event(key(KeyCode::Delete));
        assert!(sent(&mut commands).is_empty());
        assert!(matches!(info(&app).confirm, Some(Confirm::Remove(_))));
        find(&screen(&mut app, &mut terminal), "Del again to remove");
        app.on_event(key(KeyCode::Esc));
        assert_eq!(info(&app).confirm, None);
        assert_eq!(info(&app).filter, "bo");
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Kick { room_id, user_id }]
                if room_id == ROOM && user_id == "@bob:example.org"
        ));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Kick,
                result: Ok(()),
            },
        );
        assert!(info(&app).busy.is_none());
        // Ourselves and those above us cannot be removed, however often Del is hit.
        app.on_event(ctrl('u'));
        type_text(&mut app, "@me");
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        assert!(info(&app).error.is_some());
        assert!(sent(&mut commands).is_empty());
        // Enter on a member starts a direct chat; with oneself there is none.
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        app.on_event(ctrl('u'));
        type_text(&mut app, "carol");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::DirectChat { user_id, .. }] if user_id == "@carol:example.org"
        ));
        answer_opened(&mut app, Ok(CHAT.into()));
        assert!(app.modal.is_none());
        assert_eq!(app.room().unwrap().id, CHAT);
    }

    #[test]
    fn member_buttons_by_mouse() {
        let (mut app, mut terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        click_on(&mut app, &mut terminal, "@bob:example.org");
        // Selecting a member does nothing by itself.
        assert!(sent(&mut commands).is_empty());
        assert_eq!(info(&app).selected_member().unwrap().name, "bob");
        click_on(&mut app, &mut terminal, " remove ");
        assert!(sent(&mut commands).is_empty());
        click_on(&mut app, &mut terminal, " remove ");
        assert_eq!(sent(&mut commands).len(), 1);
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Kick,
                result: Err("Could not remove: not allowed".into()),
            },
        );
        // The failure asks for fresh details.
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Info { .. }]
        ));
        find(
            &screen(&mut app, &mut terminal),
            "Could not remove: not allowed",
        );
        // No remove button where we may not remove.
        click_on(&mut app, &mut terminal, "@me:example.org");
        assert!(!screen(&mut app, &mut terminal).contains(" remove "));
        // Clicking beside the popup closes it.
        app.on_event(click(1, 1));
        assert!(app.modal.is_none());
    }

    #[test]
    fn favourite_notifications_name_and_topic() {
        let (mut app, mut terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        assert_eq!(info(&app).current(), Some(InfoRow::Favourite));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetFavourite { room_id, on: true }] if room_id == ROOM
        ));
        // Locked until the answer is there.
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Favourite(true),
                result: Ok(()),
            },
        );
        assert!(info(&app).details.as_ref().unwrap().favourite);
        // Notifications: arrows step through the three modes.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Right));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetNotify {
                level: NotifyLevel::Mentions,
                ..
            }]
        ));
        find(&screen(&mut app, &mut terminal), "Mentions only");
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Notify,
                result: Ok(()),
            },
        );
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Left));
        // Busy after the first; only one went out.
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetNotify {
                level: NotifyLevel::All,
                ..
            }]
        ));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Notify,
                result: Ok(()),
            },
        );
        // Name: edited in place; typing does not filter the members meanwhile.
        app.on_event(key(KeyCode::Down));
        assert_eq!(info(&app).current(), Some(InfoRow::Name));
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "!");
        assert_eq!(info(&app).filter, "");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetName { name, .. }] if name == "Rats!"
        ));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Name,
                result: Ok(()),
            },
        );
        // An empty name is refused here; Esc leaves the edit, not the view.
        app.on_event(key(KeyCode::Enter));
        app.on_event(ctrl('u'));
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        assert!(info(&app).error.is_some());
        app.on_event(key(KeyCode::Esc));
        assert!(info(&app).editing.is_none());
        // Topic: may be emptied.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        app.on_event(ctrl('u'));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetTopic { topic, .. }] if topic.is_empty()
        ));
        // New details replace the old ones while the view is open.
        let mut changed = details();
        changed.name = "Rats!".into();
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(changed)),
            },
        );
        assert_eq!(info(&app).name, "Rats!");
        assert_eq!(info(&app).current(), Some(InfoRow::Topic));
        // Details of another room are not ours.
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: CHAT.into(),
                result: Ok(Box::new(RoomDetails::default())),
            },
        );
        assert_eq!(info(&app).name, "Rats!");
        // F4 closes what F4 opened.
        app.on_event(key(KeyCode::F(4)));
        assert!(app.modal.is_none());
    }

    #[test]
    fn without_rights_there_is_nothing_to_edit() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_event(key(KeyCode::F(4)));
        sent(&mut commands);
        let mut plain = details();
        plain.can_rename = false;
        plain.can_set_topic = false;
        plain.can_invite = false;
        for member in &mut plain.members {
            member.removable = false;
        }
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(plain)),
            },
        );
        let text = screen(&mut app, &mut terminal);
        for absent in ["Rename", "Change topic", "Invite someone"] {
            assert!(!text.contains(absent), "{absent}");
        }
        type_text(&mut app, "bob");
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn mouse_entry_points() {
        let (mut app, mut terminal, mut commands) = connected();
        // The + in the room list's title.
        let text = screen(&mut app, &mut terminal);
        let (column, row) = find(&text, " + ");
        assert_eq!(row, 0);
        app.on_event(click(column + 1, row));
        assert_eq!(new_popup(&app).stage, Stage::Choose);
        click_on(&mut app, &mut terminal, "Create a room");
        assert_eq!(new_popup(&app).stage, Stage::Create);
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        // The room's name above the messages.
        let text = screen(&mut app, &mut terminal);
        let (column, row) = find(&text, "#Rats");
        app.on_event(click(column, row));
        assert_eq!(info(&app).room_id, ROOM);
        assert_eq!(sent(&mut commands).len(), 1);
        // The wheel scrolls the view without touching what is behind it.
        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 50,
            row: 15,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(info(&app).scroll, 2);
        assert_eq!(app.timeline.scroll, 0);
    }

    #[test]
    fn palette_entries_lead_to_the_same_places() {
        let (mut app, _terminal, mut commands) = connected();
        for (query, check) in [
            ("join a room", Stage::Join),
            ("direct chat", Stage::Direct),
            ("create a room", Stage::Create),
            ("invite", Stage::Invite),
            ("new chat", Stage::Choose),
        ] {
            app.on_event(ctrl('p'));
            type_text(&mut app, query);
            app.on_event(key(KeyCode::Enter));
            assert_eq!(new_popup(&app).stage, check, "{query}");
            app.on_event(key(KeyCode::Esc));
            assert!(app.modal.is_none(), "{query}");
        }
        app.on_event(ctrl('p'));
        type_text(&mut app, "leave room");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).current(), Some(InfoRow::Leave));
        assert_eq!(info(&app).confirm, None);
        app.on_event(key(KeyCode::Esc));
        app.on_event(ctrl('p'));
        type_text(&mut app, "room info");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).current(), Some(InfoRow::Favourite));
        app.on_event(key(KeyCode::Esc));
        sent(&mut commands);
        app.on_event(ctrl('p'));
        type_text(&mut app, "favourite");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetFavourite { on: true, .. }]
        ));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Favourite(true),
                result: Ok(()),
            },
        );
        assert_eq!(app.toast.as_ref().unwrap().0, "Added to favourites");
    }

    #[test]
    fn answers_without_a_popup_do_not_get_lost_or_reopen_anything() {
        let (mut app, _terminal, mut commands) = connected();
        // The form was closed while the server was still busy.
        app.perform(Action::Manage(Click::Stage(Stage::Join)));
        type_text(&mut app, "#slow:example.org");
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        sent(&mut commands);
        answer_opened(&mut app, Err("Could not join: not allowed".into()));
        assert_eq!(app.toast.as_ref().unwrap().0, "Could not join: not allowed");
        // Another popup is not closed by a late answer.
        app.perform(Action::OpenSettings);
        answer_opened(&mut app, Ok(CHAT.into()));
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
        // Nor does it take the user anywhere: the form was closed.
        assert_eq!(app.room().unwrap().id, ROOM);
        assert_eq!(
            app.toast.as_ref().unwrap().0,
            "Done: the room is in your room list"
        );
        // Nor does one popup open over another.
        app.perform(Action::NewRoom);
        app.perform(Action::RoomInfo);
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
    }

    #[test]
    fn the_demo_explains_itself() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.perform(Action::RoomInfo);
        let text = screen(&mut app, &mut terminal);
        find(&text, "@alice:example.org");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).error.as_deref(), Some(NO_ACCOUNT));
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Manage(Click::Stage(Stage::Join)));
        type_text(&mut app, "#rust:matrix.org");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(new_popup(&app).error.as_deref(), Some(NO_ACCOUNT));
        assert!(new_popup(&app).busy.is_none());
    }

    #[test]
    fn text_from_the_network_cannot_reach_the_terminal_raw() {
        // The worker makes names and topics safe; errors pass through the same
        // helpers. What is typed is validated before it is sent anywhere.
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::Manage(Click::Stage(Stage::Join)));
        app.on_event(Event::Paste("#room:example.org\u{1b}[2J".into()));
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn snapshots() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_event(ctrl('n'));
        insta::assert_snapshot!("new_chooser", screen(&mut app, &mut terminal));
        app.on_event(key(KeyCode::Char('3')));
        type_text(&mut app, "Rat plans");
        insta::assert_snapshot!("new_create", screen(&mut app, &mut terminal));
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        with_info(&mut app, &mut commands);
        insta::assert_snapshot!("room_info", screen(&mut app, &mut terminal));
        type_text(&mut app, "bo");
        app.on_event(key(KeyCode::Delete));
        insta::assert_snapshot!("room_info_filtered", screen(&mut app, &mut terminal));
        let mut small = Terminal::new(TestBackend::new(46, 14)).unwrap();
        insta::assert_snapshot!("room_info_small", screen(&mut app, &mut small));
    }

    fn chat_request(command: &ManageCmd) -> (u64, String, bool) {
        match command {
            ManageCmd::DirectChat {
                request,
                user_id,
                create,
            } => (*request, user_id.clone(), *create),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_new_chat_from_a_member_row_asks_first() {
        let (mut app, mut terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        type_text(&mut app, "carol");
        // What Enter does is on the border and on the bottom line.
        let text = screen(&mut app, &mut terminal);
        find(&text, "Enter chat");
        find(&text, "Enter: chat");
        // The first Enter only looks for a chat that exists.
        app.on_event(key(KeyCode::Enter));
        let asked = sent(&mut commands);
        let (request, user, create) = chat_request(&asked[0]);
        assert_eq!((user.as_str(), create), ("@carol:example.org", false));
        // There is none: nothing was made, and the question is on screen.
        answer(
            &mut app,
            ManageEvent::NoDirectChat {
                request,
                user_id: user.clone(),
            },
        );
        assert_eq!(info(&app).confirm, Some(Confirm::NewChat(user.clone())));
        assert!(info(&app).busy.is_none());
        find(
            &screen(&mut app, &mut terminal),
            "Enter again to start one, Esc to stay",
        );
        assert!(sent(&mut commands).is_empty());
        // Esc, or walking away, withdraws it: the next Enter asks afresh.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(info(&app).confirm, None);
        app.on_event(key(KeyCode::Enter));
        let (request, _, create) = chat_request(&sent(&mut commands)[0]);
        assert!(!create);
        answer(
            &mut app,
            ManageEvent::NoDirectChat {
                request,
                user_id: user.clone(),
            },
        );
        // The second Enter makes the chat.
        app.on_event(key(KeyCode::Enter));
        let (_, made_for, create) = chat_request(&sent(&mut commands)[0]);
        assert!(create && made_for == user);
        find(&screen(&mut app, &mut terminal), "Starting the chat…");
        answer_opened(&mut app, Ok(CHAT.into()));
        assert!(app.modal.is_none());
        assert_eq!(app.room().unwrap().id, CHAT);
    }

    #[test]
    fn the_chat_button_asks_first_too_and_the_question_finds_its_row() {
        let (mut app, mut terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        click_on(&mut app, &mut terminal, "@bob:example.org");
        click_on(&mut app, &mut terminal, " chat ");
        let (request, user, create) = chat_request(&sent(&mut commands)[0]);
        assert!(!create);
        // The selection wanders off while the answer is on its way.
        app.on_event(key(KeyCode::Up));
        answer(
            &mut app,
            ManageEvent::NoDirectChat {
                request,
                user_id: user.clone(),
            },
        );
        assert_eq!(info(&app).selected_member().unwrap().user_id, user);
        click_on(&mut app, &mut terminal, " chat ");
        assert!(chat_request(&sent(&mut commands)[0]).2);
        // An answer about someone who is no longer listed asks nothing.
        answer_opened(
            &mut app,
            Err("Could not start the chat: not allowed".into()),
        );
        app.on_event(key(KeyCode::Enter));
        let (request, _, _) = chat_request(&sent(&mut commands)[0]);
        answer(
            &mut app,
            ManageEvent::NoDirectChat {
                request,
                user_id: "@gone:example.org".into(),
            },
        );
        assert_eq!(info(&app).confirm, None);
        assert!(info(&app).busy.is_none());
    }

    #[test]
    fn late_answers_touch_only_who_asked() {
        let (mut app, _terminal, mut commands) = connected();
        // A slow join whose form was closed.
        app.perform(Action::Manage(Click::Stage(Stage::Join)));
        type_text(&mut app, "#slow:example.org");
        app.on_event(key(KeyCode::Enter));
        let old = app.manage.requests;
        app.on_event(key(KeyCode::Esc));
        sent(&mut commands);
        // Meanwhile the room info is busy with something else.
        with_info(&mut app, &mut commands);
        type_text(&mut app, "bob");
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(info(&app).busy, Some("Removing…"));
        // The join comes through: nothing closes, nothing jumps, the wait goes on.
        answer(
            &mut app,
            ManageEvent::Opened {
                request: old,
                result: Ok("!slow:example.org".into()),
            },
        );
        assert_eq!(info(&app).busy, Some("Removing…"));
        assert!(app.manage.pending_open.is_none());
        // Or it fails: said in passing, not in the view of another room.
        answer(
            &mut app,
            ManageEvent::Opened {
                request: old,
                result: Err("Could not join: not allowed".into()),
            },
        );
        assert!(info(&app).error.is_none() && info(&app).busy.is_some());
        assert_eq!(app.toast.as_ref().unwrap().0, "Could not join: not allowed");
        // Answers about other changes or other rooms are not the one awaited.
        for (room_id, change) in [(ROOM, Change::Name), (CHAT, Change::Kick)] {
            answer(
                &mut app,
                ManageEvent::Done {
                    room_id: room_id.into(),
                    change,
                    result: Err("no".into()),
                },
            );
            assert!(info(&app).busy.is_some() && info(&app).error.is_none());
        }
        answer(
            &mut app,
            ManageEvent::Left {
                room_id: CHAT.into(),
                result: Ok(()),
            },
        );
        assert_eq!(info(&app).busy, Some("Removing…"));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Kick,
                result: Ok(()),
            },
        );
        assert!(info(&app).busy.is_none());
        // A second form does not take the first one's answer for its own.
        app.on_event(key(KeyCode::F(4)));
        app.perform(Action::Manage(Click::Stage(Stage::Join)));
        type_text(&mut app, "#other:example.org");
        app.on_event(key(KeyCode::Enter));
        answer(
            &mut app,
            ManageEvent::Opened {
                request: old,
                result: Err("Could not join: not found".into()),
            },
        );
        assert!(new_popup(&app).busy.is_some() && new_popup(&app).error.is_none());
        answer_opened(&mut app, Err("Could not join: not found".into()));
        assert!(new_popup(&app).busy.is_none() && new_popup(&app).error.is_some());
    }

    fn with_invite(app: &mut App) {
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
            summary("!inv:example.org", "Party", RoomCategory::Invite),
        ]));
    }

    /// Title and whether it can be run, of the palette entry starting with `start`.
    fn entry(app: &App, start: &str) -> (String, bool) {
        let Some(Modal::Palette(palette)) = &app.modal else {
            panic!("the palette is not open");
        };
        let entry = palette
            .entries
            .iter()
            .find(|entry| entry.title.starts_with(start))
            .unwrap_or_else(|| panic!("no entry starts with {start:?}"));
        (entry.title.clone(), entry.enabled)
    }

    #[test]
    fn the_palette_names_the_room_its_entries_act_on() {
        let (mut app, _terminal, mut commands) = connected();
        with_invite(&mut app);
        // Rats is open; the cursor of the focused room list is on the invitation.
        app.perform(Action::Focus(Pane::Sidebar));
        while app.cursor_room().map(|i| app.rooms[i].id.as_str()) != Some("!inv:example.org") {
            app.perform(Action::SidebarUp);
        }
        assert_eq!(app.room().unwrap().id, ROOM);
        app.on_event(ctrl('p'));
        assert_eq!(
            entry(&app, "Decline"),
            ("Decline the invitation to Party…".to_owned(), true)
        );
        assert!(!entry(&app, "Invite someone to Party").1);
        assert!(!entry(&app, "Favourite on/off for Party").1);
        assert!(entry(&app, "Room info").1);
        // Running it acts on that room, as it says.
        type_text(&mut app, "decline the invitation");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).room_id, "!inv:example.org");
        assert_eq!(info(&app).current(), Some(InfoRow::Leave));
        app.on_event(key(KeyCode::Esc));
        sent(&mut commands);

        // The other way round: an invitation is open, the cursor is on a room.
        app.perform(Action::SidebarOpen);
        assert_eq!(app.room().unwrap().id, "!inv:example.org");
        app.perform(Action::Focus(Pane::Sidebar));
        app.perform(Action::SidebarDown);
        let under_cursor = app.rooms[app.cursor_room().unwrap()].clone();
        assert_ne!(under_cursor.id, "!inv:example.org");
        app.on_event(ctrl('p'));
        let (title, enabled) = entry(&app, "Leave");
        assert_eq!(title, format!("Leave {}…", under_cursor.name));
        assert!(enabled && entry(&app, "Invite someone to").1);
        type_text(&mut app, "leave room");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).room_id, under_cursor.id);
        app.on_event(key(KeyCode::Esc));

        // With the composer focused it is the open room, by name.
        app.perform(Action::OpenRoom(
            app.rooms.iter().position(|r| r.id == ROOM).unwrap(),
        ));
        app.on_event(ctrl('p'));
        assert_eq!(entry(&app, "Leave"), ("Leave Rats…".to_owned(), true));
        assert_eq!(
            entry(&app, "Invite someone"),
            ("Invite someone to Rats…".to_owned(), true)
        );
        app.on_event(key(KeyCode::Esc));

        // A room list filtered down to nothing has no room to act on: the entries
        // say so by being off, instead of failing when run.
        app.perform(Action::FocusRoomFilter);
        type_text(&mut app, "zzzz");
        assert!(app.cursor_room().is_none());
        app.on_event(ctrl('p'));
        for start in ["Leave", "Invite someone", "Favourite on/off", "Room info"] {
            assert!(!entry(&app, start).1, "{start}");
        }
        type_text(&mut app, "room info");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
    }

    #[test]
    fn the_title_opens_the_open_rooms_info_whatever_has_the_focus() {
        let (mut app, mut terminal, mut commands) = connected();
        with_invite(&mut app);
        app.perform(Action::Focus(Pane::Sidebar));
        while app.cursor_room().map(|i| app.rooms[i].id.as_str()) != Some("!inv:example.org") {
            app.perform(Action::SidebarUp);
        }
        let text = screen(&mut app, &mut terminal);
        let (column, row) = find(&text, "#Rats");
        app.on_event(click(column, row));
        assert_eq!(info(&app).room_id, ROOM);
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Info { room_id }] if room_id == ROOM
        ));
        app.on_event(key(KeyCode::Esc));
        // F4 in the room list is about the room under the cursor.
        app.on_event(key(KeyCode::F(4)));
        assert_eq!(info(&app).room_id, "!inv:example.org");
    }

    #[test]
    fn room_info_follows_the_room_list() {
        let (mut app, mut terminal, mut commands) = connected();
        with_invite(&mut app);
        app.perform(Action::Focus(Pane::Sidebar));
        while app.cursor_room().map(|i| app.rooms[i].id.as_str()) != Some("!inv:example.org") {
            app.perform(Action::SidebarUp);
        }
        app.on_event(key(KeyCode::F(4)));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).confirm, Some(Confirm::Leave));
        find(&screen(&mut app, &mut terminal), "Decline invitation");
        // Accepted elsewhere: it is a room now, and nothing stays armed.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
            summary("!inv:example.org", "Party", RoomCategory::Room),
        ]));
        assert_eq!(info(&app).confirm, None);
        assert_eq!(info(&app).rows(), [InfoRow::Favourite, InfoRow::Leave]);
        let text = screen(&mut app, &mut terminal);
        find(&text, "Leave room");
        assert!(!text.contains("Decline invitation"));
        // Gone from the list (left on another device, or removed): the view goes too.
        sent(&mut commands);
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
        ]));
        assert!(app.modal.is_none());
        assert_eq!(
            app.toast.as_ref().unwrap().0,
            "You are no longer in that room"
        );
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::StopInfo]
        ));
        // While we are leaving ourselves, the answer closes it, with the name.
        app.perform(Action::OpenRoom(0));
        app.perform(Action::LeaveRoom);
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        let leaving = info(&app).room_id.clone();
        let name = info(&app).name.clone();
        let rest: Vec<RoomSummary> = [
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
        ]
        .into_iter()
        .filter(|room| room.id != leaving)
        .collect();
        app.on_matrix(MatrixEvent::Rooms(rest));
        assert!(matches!(app.modal, Some(Modal::RoomInfo(_))));
        answer(
            &mut app,
            ManageEvent::Left {
                room_id: leaving,
                result: Ok(()),
            },
        );
        assert!(app.modal.is_none());
        assert_eq!(app.toast.as_ref().unwrap().0, format!("Left {name}"));
        // The invite form goes with its room as well.
        app.perform(Action::InviteUser);
        assert_eq!(new_popup(&app).stage, Stage::Invite);
        app.on_matrix(MatrixEvent::Rooms(Vec::new()));
        assert!(app.modal.is_none());
    }

    #[test]
    fn rights_lost_and_failed_updates_show_while_the_view_is_open() {
        let (mut app, mut terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        // In the middle of renaming, the right to do so is taken away.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "!");
        assert!(info(&app).editing.is_some());
        let mut plain = details();
        plain.can_rename = false;
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(plain)),
            },
        );
        assert!(info(&app).editing.is_none());
        find(&screen(&mut app, &mut terminal), "no longer allowed");
        // Typing goes to the member filter again, and Enter sends no rename.
        app.on_event(key(KeyCode::Enter));
        assert!(
            sent(&mut commands)
                .iter()
                .all(|c| !matches!(c, ManageCmd::SetName { .. }))
        );
        // A failed update keeps what was shown and says so.
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Err("This room is not known yet.".into()),
            },
        );
        assert_eq!(info(&app).details.as_ref().unwrap().members.len(), 4);
        find(
            &screen(&mut app, &mut terminal),
            "This room is not known yet.",
        );
        // A member list that could not be brought up to date is flagged, not emptied.
        app.on_event(key(KeyCode::Esc));
        let mut stale = details();
        stale.members_error = Some("Could not load the member list: not allowed".into());
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(stale)),
            },
        );
        info_mut(&mut app).error = None;
        type_text(&mut app, "bob");
        let text = screen(&mut app, &mut terminal);
        find(&text, "may be out of date");
        find(&text, "Could not load the member list");
        find(&text, "@bob:example.org");
        // Too large to list completely: said next to the heading.
        let mut partial = details();
        partial.members_partial = true;
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(partial)),
            },
        );
        find(&screen(&mut app, &mut terminal), "only the 4 known here");
    }

    fn info_mut(app: &mut App) -> &mut InfoPopup {
        match &mut app.modal {
            Some(Modal::RoomInfo(info)) => info,
            _ => panic!("room info is not open"),
        }
    }

    #[test]
    fn the_demo_changes_nothing_ahead_of_an_answer_that_cannot_come() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.perform(Action::RoomInfo);
        let before = info(&app).details.clone().unwrap();
        // Favourite, then the notifications in both directions, then leaving.
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Right));
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        assert!(info(&app).details.as_ref() == Some(&before));
        assert!(info(&app).busy.is_none() && info(&app).confirm.is_none());
        assert_eq!(info(&app).error.as_deref(), Some(NO_ACCOUNT));
        find(&screen(&mut app, &mut terminal), NO_ACCOUNT);
    }

    #[test]
    fn typed_and_pasted_text_is_made_safe_on_the_way_in() {
        let nasty = "a\u{1b}[31mb\u{7}\u{202e}c\u{9b}d\te";
        let clean = "a[31mbcd    e";
        let (mut app, mut terminal, mut commands) = connected();
        // Every one-line input of the forms, by paste and by key.
        for stage in [Stage::Join, Stage::Direct, Stage::Create] {
            app.perform(Action::Manage(Click::Stage(stage)));
            app.on_event(Event::Paste(format!("{nasty}\nsecond line")));
            for c in nasty.chars() {
                app.on_event(key(KeyCode::Char(c)));
            }
            let popup = new_popup(&app);
            let field = match stage {
                Stage::Join => &popup.address,
                Stage::Direct => &popup.user,
                _ => &popup.name,
            };
            assert_eq!(*field, format!("{clean}{clean}"), "{stage:?}");
            // Before Enter, as it is typed: nothing raw reaches the screen.
            let text = screen(&mut app, &mut terminal);
            assert!(
                !text.contains(['\u{1b}', '\u{7}', '\u{202e}', '\u{9b}']),
                "{stage:?}"
            );
            app.on_event(key(KeyCode::Esc));
        }
        // What is sent as a name and topic is text as well.
        app.perform(Action::Manage(Click::Stage(Stage::Create)));
        {
            let Some(Modal::New(popup)) = &mut app.modal else {
                panic!()
            };
            // However it got into the form.
            popup.name.set(" Rat\u{1b}[2J plans\u{202e} ");
            popup.topic.set("what\u{7} next");
            popup.focus = NewPopup::BUTTON;
        }
        app.on_event(key(KeyCode::Enter));
        match sent(&mut commands).as_slice() {
            [ManageCmd::Create { name, topic, .. }] => {
                assert_eq!(name, "Rat[2J plans");
                assert_eq!(topic.as_deref(), Some("what next"));
            }
            other => panic!("{other:?}"),
        }
        app.on_event(key(KeyCode::Esc));

        // The room info: the member filter and the name while it is edited.
        with_info(&mut app, &mut commands);
        app.on_event(Event::Paste(nasty.into()));
        assert_eq!(info(&app).filter, clean);
        app.on_event(ctrl('u'));
        info_mut(&mut app).select(InfoRow::Name);
        assert_eq!(info(&app).current(), Some(InfoRow::Name));
        app.on_event(key(KeyCode::Enter));
        app.on_event(Event::Paste(nasty.into()));
        for c in nasty.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        assert_eq!(
            info(&app).editing.as_ref().unwrap().1,
            format!("Rats{clean}{clean}")
        );
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains(['\u{1b}', '\u{7}', '\u{202e}', '\u{9b}']));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetName { name, .. }] if *name == format!("Rats{clean}{clean}")
        ));
        // No input grows without bound.
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Manage(Click::Stage(Stage::Join)));
        for _ in 0..40 {
            app.on_event(Event::Paste("x".repeat(100)));
        }
        assert_eq!(new_popup(&app).address.len(), FIELD_MAX);
    }

    #[test]
    fn a_hostile_room_id_is_not_drawn_raw() {
        let (mut app, mut terminal, mut commands) = connected();
        let id = "!bad\u{1b}[31m\u{202e}id:example.org";
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            id,
            "Odd",
            RoomCategory::Room,
        )]));
        app.on_event(key(KeyCode::F(4)));
        sent(&mut commands);
        for with_details in [false, true] {
            if with_details {
                answer(
                    &mut app,
                    ManageEvent::Info {
                        room_id: id.into(),
                        result: Ok(Box::new(details())),
                    },
                );
            }
            let text = screen(&mut app, &mut terminal);
            find(&text, "!bad[31mid:example.org");
            assert!(!text.contains(['\u{1b}', '\u{202e}']));
        }
    }

    #[test]
    fn a_low_window_can_be_scrolled_back_to_the_top() {
        let (mut app, _terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        let mut small = Terminal::new(TestBackend::new(46, 14)).unwrap();
        // Down into the members and all the way back up.
        for _ in 0..9 {
            app.on_event(key(KeyCode::Down));
        }
        let text = screen(&mut app, &mut small);
        assert!(!text.contains("Address"));
        for _ in 0..9 {
            app.on_event(key(KeyCode::Up));
        }
        assert_eq!(info(&app).selected, 0);
        let text = screen(&mut app, &mut small);
        // The first row is selected, but what stands above it is still out of view…
        assert!(!text.contains("│ Rats "), "{text}");
        // …until Up is pressed further.
        for _ in 0..12 {
            app.on_event(key(KeyCode::Up));
        }
        let text = screen(&mut app, &mut small);
        find(&text, "│ Rats ");
        find(&text, "Address");
        assert_eq!(info(&app).scroll, 0);
        // Home does it in one, from anywhere; Down brings the selection back in.
        app.on_event(key(KeyCode::End));
        screen(&mut app, &mut small);
        app.on_event(key(KeyCode::Home));
        find(&screen(&mut app, &mut small), "│ Rats ");
        app.on_event(key(KeyCode::Down));
        find(&screen(&mut app, &mut small), "› Notifications");
    }

    #[test]
    fn a_question_that_cannot_be_read_is_not_open() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::LeaveRoom);
        sent(&mut commands);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(info(&app).confirm, Some(Confirm::Leave));
        // Says what else leaving does.
        find(
            &screen(&mut app, &mut terminal),
            "this device forgets the room",
        );
        // Too low for the line the question stands on: withdrawn, so that a second
        // Enter is a first one again.
        let mut low = Terminal::new(TestBackend::new(60, 7)).unwrap();
        screen(&mut app, &mut low);
        assert_eq!(info(&app).confirm, None);
        app.on_event(key(KeyCode::Enter));
        screen(&mut app, &mut low);
        app.on_event(key(KeyCode::Enter));
        screen(&mut app, &mut low);
        assert!(sent(&mut commands).is_empty());
        // Too narrow for the keys of the question: the same.
        let mut narrow = Terminal::new(TestBackend::new(26, 20)).unwrap();
        app.on_event(key(KeyCode::Enter));
        screen(&mut app, &mut narrow);
        assert_eq!(info(&app).confirm, None);
        // Narrow, but the keys fit: the explanation is what gets cut.
        let mut tight = Terminal::new(TestBackend::new(46, 20)).unwrap();
        app.on_event(key(KeyCode::Enter));
        let text = screen(&mut app, &mut tight);
        find(&text, "Enter again to leave, Esc to stay");
        assert_eq!(info(&app).confirm, Some(Confirm::Leave));
        // An error that arrives meanwhile does not push the question off the screen.
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Err("This room is not known yet.".into()),
            },
        );
        find(
            &screen(&mut app, &mut tight),
            "Enter again to leave, Esc to stay",
        );
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands).len(), 1);
    }

    #[test]
    fn long_filters_and_narrow_rows_stay_inside_the_popup() {
        let (mut app, _terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        let mut list = details();
        list.members[1].invited = true;
        list.topic = "x".repeat(74 * 9);
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(list)),
            },
        );
        for (width, height) in [(100, 30), (46, 14), (30, 12), (22, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            app.on_event(ctrl('u'));
            let text = screen(&mut app, &mut terminal);
            // A cut topic ends in one sign, not two.
            assert!(!text.contains("……"), "{width}x{height}");
            type_text(&mut app, &"é".repeat(90));
            terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            let popup = width.min(74 + 2);
            let (left, right) = ((width - popup.min(width)) / 2, (width + popup) / 2);
            assert!(
                cursor.x >= left && cursor.x < right.min(width),
                "{width}x{height}: cursor at {}",
                cursor.x
            );
            // The end of the filter is what shows, with the cursor right behind it.
            let text = format!("{}", terminal.backend());
            let line = text.lines().nth(cursor.y as usize).unwrap_or_default();
            assert!(line.contains("éé"), "{width}x{height}: {line}");
        }
    }

    #[test]
    fn manage_forms_and_room_info_edit_mid_text_and_click_to_place() {
        let (mut app, mut terminal, mut commands) = connected();
        let ctrl_key = |code: KeyCode| Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL));
        let alt = |c: char| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT));

        // 1. NewPopup (Stage::Create): mid-text edit and click-to-place incl. #alias offset.
        app.on_event(ctrl('n'));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter)); // Create stage
        assert_eq!(new_popup(&app).stage, Stage::Create);
        type_text(&mut app, "rats👍🏽 club");
        app.on_event(ctrl_key(KeyCode::Left)); // before "club"
        app.on_event(key(KeyCode::Left)); // before ' ' -> after 👍🏽
        app.on_event(key(KeyCode::Backspace)); // removes whole 👍🏽, keeping the space
        app.on_event(alt('f')); // after "club"
        app.on_event(ctrl('w')); // removes "club"
        app.on_event(alt('b'));
        app.on_event(ctrl('k'));
        assert_eq!(new_popup(&app).name, "");
        app.on_event(ctrl('u'));
        type_text(&mut app, "rats 👍🏽 club");
        app.on_event(ctrl_key(KeyCode::Left)); // before "club"
        app.on_event(key(KeyCode::Left)); // before ' '
        app.on_event(key(KeyCode::Backspace)); // removes whole 👍🏽 -> "rats  club"
        assert_eq!(new_popup(&app).name, "rats  club");
        app.on_event(ctrl('k')); // removes " club" after cursor
        assert_eq!(new_popup(&app).name, "rats ");

        // Switch to public and edit alias; click on 'b' after "#alpha-".
        app.on_event(key(KeyCode::Tab)); // Topic
        app.on_event(key(KeyCode::Tab)); // Access
        app.on_event(key(KeyCode::Char(' '))); // public = true
        app.on_event(key(KeyCode::Tab)); // Address / alias
        type_text(&mut app, "alpha-beta");
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "#alpha-beta");
        app.on_event(click(col + 1 + 6, row)); // right at 'b' (skip '#' + "alpha-")
        app.on_event(key(KeyCode::Delete));
        assert_eq!(new_popup(&app).alias, "alpha-eta");
        app.on_event(key(KeyCode::End));
        app.on_event(ctrl('w')); // removes "eta" from the right end
        assert_eq!(new_popup(&app).alias, "alpha-");
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));

        // 2. InfoPopup editing + filter collision policy + click-to-place.
        with_info(&mut app, &mut commands);
        // Move to Rename row and enter inline editing.
        let rename_pos = info(&app)
            .rows()
            .iter()
            .position(|r| *r == InfoRow::Name)
            .unwrap();
        for _ in 0..rename_pos {
            app.on_event(key(KeyCode::Down));
        }
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(info(&app).editing, Some((InfoRow::Name, _))));
        app.on_event(ctrl('u'));
        type_text(&mut app, "Big Rats");
        app.on_event(ctrl('a'));
        app.on_event(key(KeyCode::Char('*')));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "*Big Rats");
        app.on_event(click(col + 4, row)); // after "Big" in "*Big Rats" -> before ' '
        app.on_event(key(KeyCode::Delete));
        assert_eq!(
            info(&app).editing.as_ref().map(|(_, t)| t.as_str()),
            Some("*BigRats")
        );
        app.on_event(key(KeyCode::Esc)); // close inline edit

        // Collision policy for InfoPopup.filter:
        // (a) On Notify row: plain Left/Right change Notify level; Ctrl-Left/Ctrl-E edit filter.
        type_text(&mut app, "qq");
        let notify_pos = info(&app)
            .rows()
            .iter()
            .position(|r| *r == InfoRow::Notify)
            .unwrap();
        app.on_event(key(KeyCode::Home));
        for _ in 0..notify_pos {
            app.on_event(key(KeyCode::Down));
        }
        assert_eq!(info(&app).current(), Some(InfoRow::Notify));
        let _ = sent(&mut commands);
        app.on_event(key(KeyCode::Right));
        assert!(!sent(&mut commands).is_empty());
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Notify,
                result: Ok(()),
            },
        );
        assert_eq!(info(&app).filter, "qq");
        assert_eq!(info(&app).filter.cursor(), 2);
        app.on_event(ctrl_key(KeyCode::Left));
        assert_eq!(info(&app).filter.cursor(), 0);
        app.on_event(ctrl('e'));
        app.on_event(key(KeyCode::Char('z')));
        assert_eq!(info(&app).filter, "qqz");

        // (b) On Favourite row (index 0): Home/End step rows, while Delete/Left/Right edit filter.
        app.on_event(key(KeyCode::End));
        let last_idx = info(&app).rows().len() - 1;
        assert_eq!(info(&app).selected, last_idx);
        app.on_event(key(KeyCode::Home));
        assert_eq!(info(&app).selected, 0);
        app.on_event(key(KeyCode::Left)); // "qq|z"
        app.on_event(key(KeyCode::Delete)); // "qq"
        assert_eq!(info(&app).filter, "qq");

        // (c) Click rendered filter text (HitId::Input(InfoFilter)) places filter cursor.
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "qq");
        app.on_event(click(col + 1, row)); // q|q
        app.on_event(key(KeyCode::Char('Y')));
        assert_eq!(info(&app).filter, "qYq");

        // (d) On Member(_) row: Delete confirms member removal instead of editing filter.
        app.on_event(ctrl('u'));
        type_text(&mut app, "bo"); // matches member bob
        app.on_event(key(KeyCode::End)); // lands on bob (last visible row)
        assert!(matches!(info(&app).current(), Some(InfoRow::Member(_))));
        app.on_event(key(KeyCode::Delete));
        assert!(info(&app).confirm.is_some());
        assert_eq!(info(&app).filter, "bo");
    }
}
