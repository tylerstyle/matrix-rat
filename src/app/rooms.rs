//! Room management on `App`: joining, direct chats, creating, inviting, leaving, and
//! the room info view. The popups' state lives in `crate::manage`.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};

use super::{
    App, PathCompleteResult, bump_search_gen, complete_path_field, dropped_single_image_path,
    search_debounce_deadline, take_due_search_debounce, validate_avatar_path,
};
use crate::action::{Action, Pane};
use crate::lineedit::{EditKeys, LineEdit};
use crate::manage::{
    Address, BanPrompt, Click, Confirm, FIELD_MAX, InfoPopup, InfoRow, NAME_MAX, NewPopup, Pending,
    PermAction, PreviewButton, RoleChooser, RoomSetting, SettingSubview, Stage, address_entries,
    clean_input, normalize_alias_input, parse_address, parse_alias_local, parse_user,
    validate_permissions_draft,
};
use crate::matrix::{
    Change, DirectoryPage, ManageCmd, ManageEvent, MatrixCmd, MemberRole, NotifyLevel,
    PreviewMembership, RoomDetails, RoomHistory, RoomJoinRule, RoomMember,
};
use crate::modal::Modal;
use crate::model::{MsgKind, RoomKind};
use crate::text::sanitize_line;

/// A room that was joined or created shows up in the room list a moment later. Longer
/// than this and opening it by itself would come as a surprise.
pub(super) const OPEN_WITHIN: Duration = Duration::from_secs(60);
pub(super) const NO_ACCOUNT: &str = "The demo has no account to do this with.";

#[derive(Default)]
pub struct ManageState {
    /// The room to open as soon as the room list has it, and since when.
    pending_open: Option<(String, Instant)>,
    /// Numbers the requests whose answers have to find the popup that asked.
    requests: u64,
    /// The space to select as soon as the room list has it, and since when.
    pub(super) pending_space: Option<(String, Instant)>,
    /// The demo's suggested rooms, by space and room id.
    pub(super) demo_suggested: Vec<(String, String)>,
}

/// Hand-written: the rooms and spaces waited for are named by their ids.
impl std::fmt::Debug for ManageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManageState")
            .field("pending_open", &self.pending_open.is_some())
            .field("requests", &self.requests)
            .field("pending_space", &self.pending_space.is_some())
            .field("demo_suggested", &self.demo_suggested.len())
            .finish()
    }
}

impl ManageState {
    pub(super) fn record_pending_open(&mut self, room_id: String) {
        self.pending_open = Some((room_id, Instant::now()));
    }

    /// The number of the request made last.
    #[cfg(test)]
    pub(super) fn requests_for_test(&self) -> u64 {
        self.requests
    }
}

impl App {
    /// The room that room-level commands are about: the one under the cursor while
    /// the room list has the focus, otherwise the open one. The command palette
    /// describes its entries by the same rule.
    pub(super) fn target_room(&self) -> Option<usize> {
        if self.focus == Pane::Sidebar {
            self.cursor_room()
        } else {
            self.open_index()
        }
    }

    /// Name and kind of the room the room-level commands would act on now.
    pub(super) fn manage_target(&self) -> Option<(String, RoomKind)> {
        let room = self.rooms.get(self.target_room()?)?;
        Some((room.name.clone(), room.kind))
    }

    /// Sends a command to the worker; false in the demo, which has none.
    pub(super) fn manage(&mut self, command: ManageCmd) -> bool {
        if self.matrix.is_none() {
            return false;
        }
        self.send_matrix(MatrixCmd::Manage(command));
        true
    }

    pub(super) fn next_request(&mut self) -> u64 {
        self.manage.requests += 1;
        self.manage.requests
    }

    pub(super) fn perform_manage(&mut self, action: Action) {
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "only called from App::perform for room-management actions"
        )]
        match action {
            Action::NewRoom => self.open_new(Stage::Choose),
            Action::InviteUser => self.open_new(Stage::Invite),
            Action::RoomInfo => self.open_info(self.target_room(), None),
            Action::SpaceInfo => self.open_selected_space_info(),
            Action::LeaveRoom => self.open_info(self.target_room(), Some(InfoRow::Leave)),
            Action::Manage(click) => self.manage_click(click),
            _ => {}
        }
    }

    // ----- new chat or room ----------------------------------------------------------

    fn open_new(&mut self, stage: Stage) {
        self.open_new_with(stage, |_| {});
    }

    pub(super) fn open_room_preview(&mut self, target: &str, via: Vec<String>) {
        let request = self.next_request();
        let has_matrix = self.matrix.is_some();
        let owned_target = target.to_owned();
        let via_clone = via.clone();
        self.open_new_with(Stage::Preview, move |popup| {
            Self::prepare_preview_popup(
                popup,
                owned_target,
                via_clone,
                false,
                has_matrix.then_some(request),
            );
        });
        if has_matrix {
            self.manage(ManageCmd::Preview {
                request,
                target: target.to_owned(),
                via,
            });
        }
    }

    fn prepare_preview_popup(
        popup: &mut NewPopup,
        target: String,
        via: Vec<String>,
        from_browse: bool,
        request: Option<u64>,
    ) {
        popup.preview_from_browse = from_browse;
        popup.preview_target = target;
        popup.preview_via = via;
        popup.preview = None;
        popup.preview_reason.set("");
        popup.preview_loading = request.is_some();
        popup.preview_request = request;
        popup.error = None;
        popup.busy = None;
        popup.focus = popup.default_preview_focus();
    }

    pub(super) fn open_new_with(&mut self, stage: Stage, fill: impl FnOnce(&mut NewPopup)) {
        // From the room info view the form takes its place; any other popup stays.
        let from_info = match &self.modal {
            Some(Modal::RoomInfo(info)) => Some(info.room_id.clone()),
            Some(Modal::New(_)) | None => None,
            Some(_) => return,
        };
        // A space is not in the room list: its info knows what to invite to.
        let space = match &self.modal {
            Some(Modal::RoomInfo(info)) if info.space => {
                Some((info.room_id.clone(), info.name.clone()))
            }
            _ => None,
        };
        if self.images.viewer_open() {
            return;
        }
        if matches!(self.modal, Some(Modal::New(ref popup)) if popup.busy.is_none()) {
            self.cancel_user_search();
            self.cancel_dir_search();
        }
        if let Some(Modal::New(popup)) = &mut self.modal {
            if popup.busy.is_none() {
                popup.go(stage);
                fill(popup);
                if stage == Stage::Browse {
                    self.init_dir_browse();
                }
            }
            return;
        }
        let mut popup = NewPopup::new(stage);
        fill(&mut popup);
        if stage == Stage::Invite
            && let Some(space) = space
        {
            popup.room = Some(space);
            popup.into_space = true;
        } else if stage == Stage::Invite {
            let room = match &from_info {
                Some(id) => self.rooms.iter().find(|room| room.id == *id),
                None => self.target_room().and_then(|index| self.rooms.get(index)),
            };
            let Some(room) = room else {
                self.show_toast("No room to invite to");
                return;
            };
            if room.is_pending() {
                self.show_toast(room.kind.pending_hint());
                return;
            }
            popup.room = Some((room.id.clone(), room.name.clone()));
        }
        if from_info.is_some() {
            self.close_manage();
        }
        self.open_modal(Modal::New(Box::new(popup)));
        if stage == Stage::Browse {
            self.init_dir_browse();
        }
    }

    pub(super) fn submit_new(&mut self) {
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
            Stage::Browse => {
                if self.dir_search_debounce.take().is_some() {
                    self.dispatch_armed_dir_search();
                } else if popup.dir_results.is_empty() && !popup.dir_searching {
                    self.start_dir_search();
                } else {
                    self.preview_dir_entry();
                }
                return;
            }
            Stage::Preview => {
                if let Some(btn) = popup.preview_button_at(popup.focus) {
                    self.submit_preview_button(btn);
                }
                return;
            }
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
            Stage::CreateSpace => (|| {
                let name = clean_input(popup.name.trim());
                if name.is_empty() {
                    return Err("Give the space a name.".to_owned());
                }
                if name.len() > NAME_MAX {
                    return Err("That name is too long.".to_owned());
                }
                let alias = if !popup.public {
                    None
                } else if popup.alias.trim().trim_start_matches('#').is_empty() {
                    return Err("A public space needs an address.".to_owned());
                } else {
                    Some(parse_alias_local(&popup.alias)?)
                };
                let topic = clean_input(popup.topic.trim());
                Ok((
                    ManageCmd::CreateSpace {
                        request,
                        name,
                        topic: (!topic.is_empty()).then_some(topic),
                        alias,
                    },
                    "Creating the space…",
                ))
            })(),
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
                self.cancel_dir_search();
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
            let mut entered = false;
            #[expect(
                clippy::wildcard_enum_match_arm,
                reason = "foreign crossterm::event::KeyCode"
            )]
            match code {
                KeyCode::Up | KeyCode::BackTab => popup.step(-1),
                KeyCode::Down | KeyCode::Tab => popup.step(1),
                KeyCode::Enter | KeyCode::Right => {
                    popup.go(Stage::CHOICES[popup.choice]);
                    entered = true;
                }
                KeyCode::Char(c @ '1'..='5') if plain => {
                    popup.choice = c as usize - '1' as usize;
                    popup.go(Stage::CHOICES[popup.choice]);
                    entered = true;
                }
                _ => {}
            }
            if entered && matches!(&self.modal, Some(Modal::New(p)) if p.stage == Stage::Browse) {
                self.init_dir_browse();
            }
            return;
        }
        if popup.stage == Stage::Browse {
            #[expect(
                clippy::wildcard_enum_match_arm,
                reason = "foreign crossterm::event::KeyCode"
            )]
            match code {
                KeyCode::Up => {
                    popup.step_dir(-1);
                    self.maybe_load_more_dir();
                    return;
                }
                KeyCode::Down => {
                    popup.step_dir(1);
                    self.maybe_load_more_dir();
                    return;
                }
                KeyCode::PageUp => {
                    let page = popup.dir_page.max(1) as isize;
                    popup.step_dir(-page);
                    self.maybe_load_more_dir();
                    return;
                }
                KeyCode::PageDown => {
                    let page = popup.dir_page.max(1) as isize;
                    popup.step_dir(page);
                    self.maybe_load_more_dir();
                    return;
                }
                KeyCode::Tab => {
                    popup.step(1);
                    return;
                }
                KeyCode::BackTab => {
                    popup.step(-1);
                    return;
                }
                KeyCode::Enter => {
                    self.submit_new();
                    return;
                }
                _ => {
                    if let Some(field) = popup.field_mut()
                        && field.handle_key(code, mods, EditKeys::FULL).changed()
                    {
                        popup.error = None;
                        self.on_dir_search_input_changed();
                    }
                    return;
                }
            }
        }
        if popup.stage == Stage::Preview {
            #[expect(
                clippy::wildcard_enum_match_arm,
                reason = "foreign crossterm::event::KeyCode"
            )]
            match code {
                KeyCode::Tab | KeyCode::Down => {
                    popup.step(1);
                    return;
                }
                KeyCode::BackTab | KeyCode::Up => {
                    popup.step(-1);
                    return;
                }
                KeyCode::Left if popup.field_mut().is_none() && plain => {
                    popup.step(-1);
                    return;
                }
                KeyCode::Right if popup.field_mut().is_none() && plain => {
                    popup.step(1);
                    return;
                }
                KeyCode::Enter => {
                    if let Some(btn) = popup.preview_button_at(popup.focus) {
                        self.submit_preview_button(btn);
                    } else if popup.focus + 1 < popup.slots() {
                        popup.step(1);
                    }
                    return;
                }
                _ => {
                    if let Some(field) = popup.field_mut()
                        && field.handle_key(code, mods, EditKeys::FULL).changed()
                    {
                        popup.error = None;
                    }
                    return;
                }
            }
        }
        let search_stage = matches!(popup.stage, Stage::Direct | Stage::Invite);
        if search_stage && !popup.user_results.is_empty() {
            #[expect(
                clippy::wildcard_enum_match_arm,
                reason = "foreign crossterm::event::KeyCode"
            )]
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
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "foreign crossterm::event::KeyCode"
        )]
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

    fn own_server(&self) -> Option<String> {
        self.user_id
            .as_deref()
            .and_then(|id| id.split_once(':'))
            .map(|(_, server)| server.to_owned())
    }

    fn init_dir_browse(&mut self) {
        let default_server = self.own_server();
        if let Some(Modal::New(popup)) = &mut self.modal {
            popup.dir_default_server = default_server;
            popup.dir_query.set("");
            popup.dir_server.set("");
            popup.clear_dir_search();
            popup.error = None;
        }
        self.start_dir_search();
    }

    pub(super) fn on_dir_search_input_changed(&mut self) {
        let has_matrix = self.matrix.is_some();
        let (next_gen, was_debouncing) =
            bump_search_gen(&mut self.dir_search_gen, &mut self.dir_search_debounce);
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        let had_worker_task = (popup.dir_searching || popup.dir_loading_more) && !was_debouncing;
        if popup.stage != Stage::Browse {
            popup.clear_dir_search();
            if has_matrix && had_worker_task {
                self.send_matrix(MatrixCmd::CancelDirectorySearch);
            }
            return;
        }
        popup.clear_dir_search();
        popup.dir_search_gen = Some(next_gen);
        popup.dir_searching = has_matrix;
        if has_matrix {
            if had_worker_task {
                self.send_matrix(MatrixCmd::CancelDirectorySearch);
            }
            self.dir_search_debounce = Some(Instant::now());
        } else {
            popup.dir_error = Some(NO_ACCOUNT.to_owned());
        }
    }

    pub(super) fn cancel_dir_search(&mut self) {
        let (_, was_debouncing) =
            bump_search_gen(&mut self.dir_search_gen, &mut self.dir_search_debounce);
        let had_worker_task = match &mut self.modal {
            Some(Modal::New(popup)) => {
                let active = (popup.dir_searching || popup.dir_loading_more) && !was_debouncing;
                popup.clear_dir_search();
                active
            }
            _ => false,
        };
        if had_worker_task && self.matrix.is_some() {
            self.send_matrix(MatrixCmd::CancelDirectorySearch);
        }
    }

    pub(super) fn dir_search_debounce_deadline(&self) -> Option<tokio::time::Instant> {
        search_debounce_deadline(self.dir_search_debounce)
    }

    pub(super) fn tick_dir_search(&mut self) {
        if take_due_search_debounce(&mut self.dir_search_debounce) {
            self.dispatch_armed_dir_search();
        }
    }

    fn start_dir_search(&mut self) {
        let (generation, _) =
            bump_search_gen(&mut self.dir_search_gen, &mut self.dir_search_debounce);
        self.dispatch_dir_search(generation);
    }

    fn dispatch_armed_dir_search(&mut self) {
        self.dir_search_debounce = None;
        let generation = match &self.modal {
            Some(Modal::New(popup)) => popup.dir_search_gen,
            _ => None,
        }
        .unwrap_or(self.dir_search_gen);
        self.dispatch_dir_search(generation);
    }

    fn dispatch_dir_search(&mut self, generation: u64) {
        let has_matrix = self.matrix.is_some();
        let dispatch = match &mut self.modal {
            Some(Modal::New(popup)) if popup.stage == Stage::Browse && popup.busy.is_none() => {
                popup.clear_dir_search();
                popup.dir_search_gen = Some(generation);
                if has_matrix {
                    popup.dir_searching = true;
                    let filter = sanitize_line(popup.dir_query.as_str()).trim().to_owned();
                    let server = sanitize_line(popup.dir_server.as_str()).trim().to_owned();
                    Some((filter, (!server.is_empty()).then_some(server)))
                } else {
                    popup.dir_error = Some(NO_ACCOUNT.to_owned());
                    None
                }
            }
            _ => None,
        };
        if let Some((query, server)) = dispatch {
            self.send_matrix(MatrixCmd::SearchDirectory {
                query,
                server,
                generation,
            });
        }
    }

    fn maybe_load_more_dir(&mut self) {
        if self.matrix.is_none() {
            return;
        }
        let generation = match &mut self.modal {
            Some(Modal::New(popup))
                if popup.stage == Stage::Browse && popup.busy.is_none() && popup.near_dir_end() =>
            {
                let Some(generation) = popup.dir_search_gen else {
                    return;
                };
                popup.dir_loading_more = true;
                generation
            }
            _ => return,
        };
        self.send_matrix(MatrixCmd::DirectoryNextPage { generation });
    }

    pub(super) fn scroll_dir_browse(&mut self, lines: isize) {
        if let Some(Modal::New(popup)) = &mut self.modal
            && popup.stage == Stage::Browse
            && popup.busy.is_none()
        {
            popup.scroll_dir(lines);
            if lines > 0
                && popup.dir_has_more
                && !popup.dir_searching
                && !popup.dir_loading_more
                && popup.dir_scroll + popup.dir_page.max(1) + 3 >= popup.dir_results.len()
                && let Some(generation) = popup.dir_search_gen
                && self.matrix.is_some()
            {
                popup.dir_loading_more = true;
                self.send_matrix(MatrixCmd::DirectoryNextPage { generation });
            }
        }
    }

    pub(super) fn on_directory_results(&mut self, page: DirectoryPage) {
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        if popup.dir_search_gen != Some(page.generation) {
            return;
        }
        popup.dir_searching = false;
        popup.dir_loading_more = false;
        popup.dir_error = None;
        popup.dir_has_more = page.has_more;
        if page.append {
            for room in page.rooms {
                if !popup
                    .dir_results
                    .iter()
                    .any(|existing| existing.room_id == room.room_id)
                {
                    popup.dir_results.push(room);
                }
            }
        } else {
            popup.dir_results = page.rooms;
            popup.dir_selected = 0;
            popup.dir_scroll = 0;
            popup.dir_reveal = true;
        }
    }

    pub(super) fn on_directory_error(&mut self, generation: u64, message: String) {
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        if popup.dir_search_gen != Some(generation) {
            return;
        }
        popup.dir_searching = false;
        popup.dir_loading_more = false;
        popup.dir_has_more = false;
        popup.dir_error = Some(message);
    }

    pub(super) fn open_joined_room_index(&mut self, index: usize) {
        let Some(room) = self.rooms.get(index) else {
            return;
        };
        let room_id = room.id.clone();
        if self.space.is_some() && !self.space_rooms.contains(&room_id) {
            self.select_space(None);
        }
        self.open_room(index);
        if let Some(at) = self.visible_rooms().iter().position(|i| *i == index) {
            self.sidebar.cursor = at;
            self.sidebar.reveal_cursor = true;
        }
    }

    fn preview_dir_entry(&mut self) {
        let (entry, server_hint) = match &self.modal {
            Some(Modal::New(popup)) if popup.stage == Stage::Browse => {
                let Some(entry) = popup.dir_results.get(popup.dir_selected).cloned() else {
                    return;
                };
                let server_hint = sanitize_line(popup.dir_server.as_str()).trim().to_owned();
                (entry, server_hint)
            }
            _ => return,
        };
        if let Some(index) = self.find_joined_room(&entry.room_id).or_else(|| {
            entry
                .alias
                .as_deref()
                .and_then(|alias| self.find_joined_room(alias))
        }) {
            self.close_manage();
            self.open_joined_room_index(index);
            return;
        }
        let target = entry
            .alias
            .clone()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| entry.room_id.clone());
        let mut via = Vec::new();
        if !server_hint.is_empty() {
            via.push(server_hint);
        }
        if let Some((_, hs)) = entry.room_id.split_once(':')
            && !via.iter().any(|s| s == hs)
        {
            via.push(hs.to_owned());
        }
        let request = self.next_request();
        let has_matrix = self.matrix.is_some();
        if let Some(Modal::New(popup)) = &mut self.modal {
            popup.go(Stage::Preview);
            Self::prepare_preview_popup(
                popup,
                target.clone(),
                via.clone(),
                true,
                has_matrix.then_some(request),
            );
        }
        if has_matrix {
            self.manage(ManageCmd::Preview {
                request,
                target,
                via,
            });
        }
    }

    /// "Open" in a preview of a room or space we are in: a room opens; a space is
    /// what the room list is narrowed to, or, deeper than the list goes, its info.
    fn open_previewed(&mut self) {
        let Some(Modal::New(popup)) = &self.modal else {
            return;
        };
        let preview = popup.preview.as_ref();
        let room_id = preview.map_or_else(|| popup.preview_target.clone(), |p| p.room_id.clone());
        let room = self
            .find_joined_room(&room_id)
            .or_else(|| self.find_joined_room(&popup.preview_target));
        let listed = self.spaces.iter().position(|space| space.id == room_id);
        let space = preview
            .filter(|p| p.space)
            .map(|p| p.name.clone().unwrap_or_default());
        self.close_manage();
        if let Some(index) = room {
            self.open_joined_room_index(index);
        } else if let Some(at) = listed {
            self.select_space(Some(at));
        } else if let Some(name) = space {
            self.open_space_info(&room_id, &name, None);
        }
    }

    fn submit_preview_button(&mut self, button: PreviewButton) {
        match button {
            PreviewButton::Cancel => {
                let from_browse = matches!(
                    &self.modal,
                    Some(Modal::New(popup)) if popup.stage == Stage::Preview && popup.preview_from_browse
                );
                if from_browse {
                    if let Some(Modal::New(popup)) = &mut self.modal {
                        popup.go(Stage::Browse);
                    }
                } else {
                    self.close_manage();
                }
                return;
            }
            PreviewButton::Open => return self.open_previewed(),
            PreviewButton::Accept => {
                let room_id = match &self.modal {
                    Some(Modal::New(popup)) => popup
                        .preview
                        .as_ref()
                        .map(|p| p.room_id.clone())
                        .unwrap_or_else(|| popup.preview_target.clone()),
                    _ => return,
                };
                if let Some(room_id) = self
                    .rooms
                    .iter()
                    .find(|r| r.id == room_id && r.kind == RoomKind::Invite)
                    .map(|r| r.id.clone())
                {
                    let request = self.next_request();
                    if let Some(Modal::New(popup)) = &mut self.modal {
                        popup.busy = Some("Joining…");
                        popup.request = Some(request);
                        popup.error = None;
                    }
                    if !self.manage(ManageCmd::Join {
                        request,
                        target: room_id,
                        via: Vec::new(),
                    }) && let Some(Modal::New(popup)) = &mut self.modal
                    {
                        popup.busy = None;
                        popup.request = None;
                        popup.error = Some(NO_ACCOUNT.to_owned());
                    }
                    return;
                }
            }
            PreviewButton::Withdraw => {
                let room_id = match &self.modal {
                    Some(Modal::New(popup)) => popup
                        .preview
                        .as_ref()
                        .map(|p| p.room_id.clone())
                        .unwrap_or_else(|| popup.preview_target.clone()),
                    _ => return,
                };
                if let Some(room_id) = self
                    .rooms
                    .iter()
                    .find(|r| r.id == room_id && r.kind == RoomKind::Knock)
                    .map(|r| r.id.clone())
                {
                    self.close_manage();
                    self.send_matrix(MatrixCmd::Leave { room_id });
                    return;
                }
            }
            PreviewButton::Join | PreviewButton::Knock => {}
        }

        let request = self.next_request();
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        if popup.busy.is_some() {
            return;
        }
        let target = popup
            .preview
            .as_ref()
            .and_then(|p| {
                p.canonical_alias
                    .clone()
                    .or_else(|| Some(p.room_id.clone()))
            })
            .unwrap_or_else(|| popup.preview_target.clone());
        let via = popup.preview_via.clone();
        let (command, busy) = match button {
            PreviewButton::Knock => {
                let clean = sanitize_line(popup.preview_reason.as_str())
                    .trim()
                    .to_owned();
                let reason = (!clean.is_empty()).then_some(clean);
                (
                    ManageCmd::Knock {
                        request,
                        target,
                        via,
                        reason,
                    },
                    "Asking to join…",
                )
            }
            PreviewButton::Join
            | PreviewButton::Open
            | PreviewButton::Accept
            | PreviewButton::Withdraw
            | PreviewButton::Cancel => (
                ManageCmd::Join {
                    request,
                    target,
                    via,
                },
                "Joining…",
            ),
        };
        popup.error = None;
        popup.busy = Some(busy);
        popup.request = Some(request);
        if !self.manage(command)
            && let Some(Modal::New(popup)) = &mut self.modal
        {
            popup.busy = None;
            popup.request = None;
            popup.error = Some(NO_ACCOUNT.to_owned());
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
        let (in_space, can_add) = self.space_context(&room.id);
        popup.set_space_context(in_space, can_add);
        if let Some(row) = select {
            popup.select(row);
        }
        let room_id = room.id.clone();
        self.open_modal(Modal::RoomInfo(Box::new(popup)));
        self.manage(ManageCmd::Info { room_id });
    }

    /// Closes either room popup. What is under way on the server carries on.
    pub(super) fn close_manage(&mut self) {
        if matches!(self.modal, Some(Modal::New(_))) {
            self.cancel_user_search();
            self.cancel_dir_search();
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
        if let Some(Modal::New(popup)) = &mut self.modal
            && popup.busy.is_none()
            && popup.stage == Stage::Preview
            && popup.preview_from_browse
        {
            popup.go(Stage::Browse);
            return;
        }
        let back_to_choose = matches!(
            &self.modal,
            Some(Modal::New(popup))
                if popup.busy.is_none() && !popup.single && popup.stage != Stage::Choose
        );
        if back_to_choose {
            self.cancel_user_search();
            self.cancel_dir_search();
            if let Some(Modal::New(popup)) = &mut self.modal {
                popup.go(Stage::Choose);
            }
            return;
        }
        match &mut self.modal {
            Some(Modal::New(_)) => self.close_manage(),
            Some(Modal::RoomInfo(info)) => {
                if let Some(SettingSubview::Addresses { confirm_delete, .. }) =
                    &mut info.setting_view
                    && confirm_delete.take().is_some()
                {
                    return;
                }
                if info.ban_prompt.take().is_some()
                    || info.role_chooser.take().is_some()
                    || info.editing.take().is_some()
                    || info.setting_view.take().is_some()
                    || info.confirm.take().is_some()
                {
                    // Stepped back from typing, choosing a role, room setting, or the question.
                } else if !info.filter.is_empty() {
                    info.set_filter(String::new());
                } else {
                    self.close_manage();
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_manage_paste(&mut self, full_text: &str, first_line: &str) {
        match &mut self.modal {
            Some(Modal::New(popup)) if popup.busy.is_none() => {
                let search_stage = matches!(popup.stage, Stage::Direct | Stage::Invite);
                let browse_stage = popup.stage == Stage::Browse;
                if let Some(field) = popup.field_mut()
                    && field.paste(first_line).changed()
                {
                    popup.error = None;
                    if search_stage {
                        self.on_user_search_input_changed();
                    } else if browse_stage {
                        self.on_dir_search_input_changed();
                    }
                }
            }
            Some(Modal::RoomInfo(info)) => {
                if let Some(subview) = &mut info.setting_view {
                    if info.busy.is_some() {
                        return;
                    }
                    match subview {
                        SettingSubview::EncryptConfirm {
                            confirm,
                            focus,
                            error,
                        } => {
                            if *focus == 0 && confirm.paste(first_line).changed() {
                                *error = None;
                            }
                        }
                        SettingSubview::Picture {
                            path,
                            focus,
                            error,
                            notice,
                        } => match dropped_single_image_path(full_text) {
                            Ok(Some(dropped)) => {
                                path.set(dropped);
                                *focus = 0;
                                *error = None;
                                *notice =
                                    Some("Image file ready — press Enter to upload.".to_owned());
                            }
                            Ok(None) => {
                                if *focus == 0 && path.paste(first_line).changed() {
                                    *error = None;
                                    *notice = None;
                                }
                            }
                            Err(msg) => {
                                *error = Some(msg.to_owned());
                                *notice = None;
                            }
                        },
                        SettingSubview::Addresses {
                            input,
                            focus,
                            confirm_delete,
                            error,
                            notice,
                            ..
                        } => {
                            if *focus == 0 && input.paste(first_line).changed() {
                                *confirm_delete = None;
                                *error = None;
                                *notice = None;
                            }
                        }
                        SettingSubview::JoinRule { .. }
                        | SettingSubview::History { .. }
                        | SettingSubview::Permissions { .. } => {}
                    }
                } else if let Some(prompt) = &mut info.ban_prompt {
                    prompt.reason.paste(first_line);
                } else if let Some((_, edit)) = &mut info.editing {
                    edit.paste(first_line);
                } else {
                    info.apply_filter(|f| f.paste(first_line));
                }
            }
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
        if info.setting_view.is_some() {
            return self.on_setting_view_key(code, mods);
        }
        if let Some(prompt) = &mut info.ban_prompt {
            if code == KeyCode::Enter && plain {
                self.submit_ban_prompt();
            } else {
                prompt.reason.handle_key(code, mods, EditKeys::FULL);
            }
            return;
        }
        if let Some(chooser) = &mut info.role_chooser {
            #[expect(
                clippy::wildcard_enum_match_arm,
                reason = "foreign crossterm::event::KeyCode"
            )]
            match code {
                KeyCode::Up => chooser.step(-1),
                KeyCode::Char('k') if plain => chooser.step(-1),
                KeyCode::Down => chooser.step(1),
                KeyCode::Char('j') if plain => chooser.step(1),
                KeyCode::Enter if plain => self.submit_role_chooser(),
                _ => {}
            }
            return;
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
        // While the selected row is out of view for a filter that nobody matches,
        // the keys that would act on it edit the filter.
        let blind = info.typing_blind();
        #[expect(
            clippy::wildcard_enum_match_arm,
            reason = "foreign crossterm::event::KeyCode"
        )]
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
            KeyCode::Left | KeyCode::Right if row == Some(InfoRow::Notify) && plain && !blind => {
                self.info_activate(InfoRow::Notify, if code == KeyCode::Left { -1 } else { 1 });
            }
            KeyCode::Left if row == Some(InfoRow::SettingsToggle) && plain && !blind => {
                if info.settings_open {
                    info.settings_open = false;
                    info.error = None;
                    info.notice = None;
                    info.reveal = true;
                }
            }
            KeyCode::Right if row == Some(InfoRow::SettingsToggle) && plain && !blind => {
                if !info.settings_open {
                    info.settings_open = true;
                    info.error = None;
                    info.notice = None;
                    info.reveal = true;
                }
            }
            // While a member filter is typed, a space belongs to it.
            KeyCode::Char(' ')
                if matches!(
                    row,
                    Some(InfoRow::SettingsToggle | InfoRow::Setting(RoomSetting::Direct))
                ) && plain
                    && info.filter.is_empty() =>
            {
                if let Some(row) = row {
                    self.info_activate(row, 0);
                }
            }
            // Not on a row that is out of view behind a filter nobody matches.
            KeyCode::Enter if !blind => {
                if let Some(row) = row {
                    self.info_activate(row, 0);
                }
            }
            KeyCode::Delete if matches!(row, Some(InfoRow::Member(_))) && plain && !blind => {
                self.remove_or_unban_member()
            }
            KeyCode::Delete if matches!(row, Some(InfoRow::Child(_))) && plain && !blind => {
                self.remove_child()
            }
            KeyCode::Char(' ')
                if matches!(row, Some(InfoRow::Child(_))) && plain && info.filter.is_empty() =>
            {
                self.toggle_child_suggested();
            }
            KeyCode::Delete if matches!(row, Some(InfoRow::KnockRequest(_))) && plain && !blind => {
                self.decline_knock()
            }
            KeyCode::Char('i')
                if mods == KeyModifiers::ALT && matches!(row, Some(InfoRow::Member(_))) =>
            {
                self.toggle_ignore_member();
            }
            KeyCode::Char('b')
                if mods == KeyModifiers::ALT
                    && !blind
                    && matches!(row, Some(InfoRow::Member(_) | InfoRow::KnockRequest(_))) =>
            {
                self.open_ban_prompt();
            }
            KeyCode::Char('r')
                if mods == KeyModifiers::ALT && matches!(row, Some(InfoRow::Member(_))) =>
            {
                self.open_role_chooser();
            }
            _ => {
                let keys = EditKeys {
                    arrows: blind
                        || (row != Some(InfoRow::Notify) && row != Some(InfoRow::SettingsToggle)),
                    home_end: false,
                    delete: blind
                        || !matches!(
                            row,
                            Some(InfoRow::Member(_) | InfoRow::KnockRequest(_) | InfoRow::Child(_))
                        ),
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
            Click::Submit => match &mut self.modal {
                Some(Modal::RoomInfo(info)) if info.ban_prompt.is_some() => {
                    self.submit_ban_prompt();
                }
                Some(Modal::New(popup)) if popup.busy.is_none() => {
                    popup.focus = popup.slots() - 1;
                    self.submit_new();
                }
                _ => {}
            },
            Click::BrowseRow(index) => {
                if let Some(Modal::New(popup)) = &mut self.modal
                    && popup.stage == Stage::Browse
                    && popup.busy.is_none()
                    && index < popup.dir_results.len()
                {
                    popup.dir_selected = index;
                    self.preview_dir_entry();
                }
            }
            Click::PreviewBtn(index) => {
                let btn = match &mut self.modal {
                    Some(Modal::New(popup))
                        if popup.stage == Stage::Preview && popup.busy.is_none() =>
                    {
                        let btn = popup.preview_buttons().get(index).copied();
                        if btn.is_some() {
                            popup.focus = usize::from(popup.preview_has_reason()) + index;
                        }
                        btn
                    }
                    _ => None,
                };
                if let Some(btn) = btn {
                    self.submit_preview_button(btn);
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
                info.blind = false;
                if let Click::Row(_) = click {
                    self.info_activate(row, 0);
                } else if let Click::Step(_, step) = click {
                    self.info_activate(row, step);
                }
            }
            Click::MemberChat => {
                if let Some(Modal::RoomInfo(info)) = &self.modal
                    && let Some(row @ InfoRow::Member(_)) = info.current()
                {
                    self.open_member_direct_chat(row);
                }
            }
            Click::MemberRemove => self.remove_member(),
            Click::MemberBan => self.open_ban_prompt(),
            Click::MemberUnban => self.unban_member(),
            Click::MemberRole => self.open_role_chooser(),
            Click::RoleOption(index) => self.pick_role_option(index),
            Click::MemberIgnore => self.toggle_ignore_member(),
            Click::KnockAccept => self.accept_knock(),
            Click::KnockDecline => self.decline_knock(),
            Click::KnockBan => self.open_ban_prompt(),
            Click::Favourite => self.toggle_room_tag(false),
            Click::LowPriority => self.toggle_room_tag(true),
            // The name above the messages is the open room's, wherever the focus is.
            Click::OpenInfo => {
                self.open_info(self.open_index(), None);
            }
            Click::SettingChoice(idx) => self.on_setting_choice(idx),
            Click::SettingSlot(slot) => self.on_setting_slot(slot),
            Click::AddressRow(idx) => {
                if let Some(Modal::RoomInfo(info)) = &mut self.modal
                    && info.busy.is_none()
                    && let Some(SettingSubview::Addresses {
                        selected,
                        confirm_delete,
                        error,
                        notice,
                        ..
                    }) = &mut info.setting_view
                {
                    *selected = idx;
                    *confirm_delete = None;
                    *error = None;
                    *notice = None;
                }
            }
            Click::AddressMakeMain(idx) => self.make_main_alias_at(idx),
            Click::AddressPublishAlt(idx) => self.publish_alt_alias_at(idx),
            Click::AddressUnpublish(idx) => self.unpublish_alias_at(idx),
            Click::AddressDeleteLocal(idx) => self.delete_local_alias_at(idx),
            Click::AddToSpace => self.add_target_room_to_space(),
            // Asked in the room's info, as leaving is.
            Click::RemoveFromSpace => {
                self.open_info(self.target_room(), Some(InfoRow::RemoveFromSpace));
            }
            Click::AddRoomToSpace => self.add_room_to_selected_space(),
            Click::ChildOpen => {
                if let Some(Modal::RoomInfo(info)) = &self.modal
                    && info.busy.is_none()
                    && let Some(row @ InfoRow::Child(_)) = info.current()
                {
                    self.open_child(row);
                }
            }
            Click::ChildSuggest => self.toggle_child_suggested(),
            Click::ChildRemove => self.remove_child(),
            Click::PermSet(row_idx, level) => {
                if let Some(Modal::RoomInfo(info)) = &mut self.modal
                    && info.busy.is_none()
                    && info.setting_editable(RoomSetting::Permissions)
                    && let Some(action) = PermAction::ALL.get(row_idx).copied()
                    && let Some(SettingSubview::Permissions {
                        draft,
                        selected,
                        error,
                    }) = &mut info.setting_view
                {
                    *selected = row_idx;
                    action.set(draft, level);
                    *error = None;
                }
            }
        }
    }

    fn toggle_room_tag(&mut self, low_priority: bool) {
        let Some(room) = self.target_room().and_then(|index| self.rooms.get(index)) else {
            self.show_toast("No room is open");
            return;
        };
        if room.is_pending() {
            self.show_toast(room.kind.pending_hint());
            return;
        }
        let room_id = room.id.clone();
        let command = if low_priority {
            ManageCmd::SetLowPriority {
                room_id,
                on: room.kind != RoomKind::LowPriority,
            }
        } else {
            ManageCmd::SetFavourite {
                room_id,
                on: room.kind != RoomKind::Favourite,
            }
        };
        if !self.manage(command) {
            self.show_toast(NO_ACCOUNT);
        }
    }

    /// Does what the row is for. `step` is the direction for rows with a value to
    /// change; 0 for Enter or a click.
    fn info_activate(&mut self, row: InfoRow, step: i8) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() {
            return;
        }
        info.error = None;
        info.notice = None;
        let room_id = info.room_id.clone();
        let confirmed = std::mem::take(&mut info.confirm);
        // Nothing is changed, shown as changed or asked about where there is no
        // account to do it with.
        let sends = !matches!(
            row,
            InfoRow::Name
                | InfoRow::OwnName
                | InfoRow::Topic
                | InfoRow::Invite
                | InfoRow::Pinned
                | InfoRow::SettingsToggle
                | InfoRow::Member(_)
                // The demo changes its own space.
                | InfoRow::AddToSpace
                | InfoRow::RemoveFromSpace
                | InfoRow::AddChild
                | InfoRow::Child(_)
        ) && !matches!(row, InfoRow::Setting(s) if s != RoomSetting::Direct);
        if sends && !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        let (command, pending, busy) = match row {
            InfoRow::Favourite | InfoRow::LowPriority => {
                let low_priority = row == InfoRow::LowPriority;
                let on = !info.details.as_ref().map_or(
                    if low_priority {
                        info.kind == RoomKind::LowPriority
                    } else {
                        info.kind == RoomKind::Favourite
                    },
                    |details| {
                        if low_priority {
                            details.low_priority
                        } else {
                            details.favourite
                        }
                    },
                );
                // Shown at once; the server's answer corrects it if need be.
                if let Some(details) = &mut info.details {
                    if low_priority {
                        details.low_priority = on;
                        if on {
                            details.favourite = false;
                        }
                    } else {
                        details.favourite = on;
                        if on {
                            details.low_priority = false;
                        }
                    }
                }
                let (cmd, change) = if low_priority {
                    (
                        ManageCmd::SetLowPriority { room_id, on },
                        Change::LowPriority(on),
                    )
                } else {
                    (
                        ManageCmd::SetFavourite { room_id, on },
                        Change::Favourite(on),
                    )
                };
                (cmd, Pending::Change(change), "Saving…")
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
            InfoRow::Name | InfoRow::OwnName | InfoRow::Topic => {
                let current = info
                    .details
                    .as_ref()
                    .map_or(String::new(), |details| match row {
                        InfoRow::Name => details.name.clone(),
                        InfoRow::OwnName => details.own_display_name.clone().unwrap_or_default(),
                        InfoRow::Topic
                        | InfoRow::Favourite
                        | InfoRow::LowPriority
                        | InfoRow::Pinned
                        | InfoRow::Notify
                        | InfoRow::Invite
                        | InfoRow::AddToSpace
                        | InfoRow::RemoveFromSpace
                        | InfoRow::Leave
                        | InfoRow::KnockRequest(_)
                        | InfoRow::AddChild
                        | InfoRow::Child(_)
                        | InfoRow::SettingsToggle
                        | InfoRow::Setting(_)
                        | InfoRow::Member(_) => crate::text::sanitize_line(&details.topic),
                    });
                info.editing = Some((row, LineEdit::with_text(current).with_max_bytes(FIELD_MAX)));
                return;
            }
            InfoRow::Pinned => return self.open_pinned_of(&room_id),
            InfoRow::Invite => return self.open_new(Stage::Invite),
            InfoRow::Leave => {
                if confirmed != Some(Confirm::Leave) {
                    info.confirm = Some(Confirm::Leave);
                    return;
                }
                (ManageCmd::Leave { room_id }, Pending::Leave, "Leaving…")
            }
            InfoRow::KnockRequest(_) => return self.accept_knock(),
            InfoRow::AddToSpace => return self.add_to_space_from_info(),
            InfoRow::RemoveFromSpace => {
                match &info.in_space {
                    None => return,
                    Some(space) if !space.can_edit => {
                        info.error = Some(format!(
                            "You are not allowed to take rooms out of {}.",
                            space.name
                        ));
                        return;
                    }
                    Some(_) => {}
                }
                if confirmed != Some(Confirm::RemoveFromSpace) {
                    info.confirm = Some(Confirm::RemoveFromSpace);
                    return;
                }
                return self.remove_from_selected_space();
            }
            InfoRow::AddChild => return self.add_child_from_info(),
            InfoRow::Child(_) => return self.open_child(row),
            InfoRow::SettingsToggle => {
                info.settings_open = !info.settings_open;
                info.reveal = true;
                return;
            }
            InfoRow::Setting(RoomSetting::Direct) => {
                let Some(details) = &mut info.details else {
                    return;
                };
                let next = !details.direct;
                details.direct = next;
                (
                    ManageCmd::SetDirect {
                        room_id,
                        direct: next,
                    },
                    Pending::Change(Change::Direct(next)),
                    "Saving…",
                )
            }
            InfoRow::Setting(setting) => return self.activate_room_setting(setting),
            InfoRow::Member(_) => {
                return self.open_user_info_from_room_member(row);
            }
        };
        info.wait_for(pending, busy);
        self.manage(command);
    }

    /// Fast direct-chat shortcut from Room Info's `" chat "` pill.
    fn open_member_direct_chat(&mut self, row: InfoRow) {
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
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        let Some(member) = info.member(row) else {
            return;
        };
        if member.own || own.as_deref() == Some(&member.user_id) {
            info.error = Some("That is you.".to_owned());
            return;
        }
        let user_id = member.user_id.clone();
        let confirmed = std::mem::take(&mut info.confirm);
        let create = confirmed == Some(Confirm::NewChat(user_id.clone()));
        info.wait_for(
            Pending::Chat(request),
            if create {
                "Starting the chat…"
            } else {
                "Opening the chat…"
            },
        );
        self.manage(ManageCmd::DirectChat {
            request,
            user_id,
            create,
        });
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
        self.manage(ManageCmd::Kick {
            room_id,
            user_id,
            reason: None,
        });
    }

    /// Del on a member row unbans a banned user or removes a joined/invited member.
    fn remove_or_unban_member(&mut self) {
        let banned = matches!(
            &self.modal,
            Some(Modal::RoomInfo(info))
                if info.selected_member().is_some_and(|m| m.banned)
        );
        if banned {
            self.unban_member();
        } else {
            self.remove_member();
        }
    }

    fn accept_knock(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        info.confirm = None;
        info.ban_prompt = None;
        info.role_chooser = None;
        let Some(req) = info.selected_knock_request().cloned() else {
            return;
        };
        if !req.acceptable {
            info.error = Some("You are not allowed to accept this request.".to_owned());
            return;
        }
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        info.wait_for(Pending::Change(Change::AcceptKnock), "Accepting…");
        let room_id = info.room_id.clone();
        self.manage(ManageCmd::AcceptKnock {
            room_id,
            user_id: req.user_id,
        });
    }

    /// Del on an incoming knock request: asks first, declines on the second press.
    fn decline_knock(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        info.ban_prompt = None;
        info.role_chooser = None;
        let Some(req) = info.selected_knock_request().cloned() else {
            return;
        };
        if !req.declinable {
            info.confirm = None;
            info.error = Some("You are not allowed to decline this request.".to_owned());
            return;
        }
        let user_id = req.user_id;
        if info.confirm != Some(Confirm::DeclineKnock(user_id.clone())) {
            info.confirm = Some(Confirm::DeclineKnock(user_id));
            return;
        }
        info.confirm = None;
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        info.wait_for(Pending::Change(Change::DeclineKnock), "Declining…");
        let room_id = info.room_id.clone();
        self.manage(ManageCmd::DeclineKnock { room_id, user_id });
    }

    fn open_ban_prompt(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        info.confirm = None;
        info.role_chooser = None;
        if let Some(member) = info.selected_member().cloned() {
            if !member.bannable {
                info.error = Some(if member.own {
                    "You cannot ban yourself.".to_owned()
                } else if member.banned {
                    "This user is already banned.".to_owned()
                } else {
                    "You are not allowed to ban this member.".to_owned()
                });
                return;
            }
            let name = display_user_label(&member.name, &member.user_id);
            info.ban_prompt = Some(BanPrompt::new(member.user_id, name));
        } else if let Some(req) = info.selected_knock_request().cloned() {
            if !req.bannable {
                info.error = Some("You are not allowed to ban this user.".to_owned());
                return;
            }
            let name = display_user_label(&req.name, &req.user_id);
            info.ban_prompt = Some(BanPrompt::for_knock(req.user_id, name));
        }
    }

    fn submit_ban_prompt(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        let Some(prompt) = info.ban_prompt.take() else {
            return;
        };
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        let reason = {
            let trimmed = sanitize_line(prompt.reason.as_str()).trim().to_owned();
            (!trimmed.is_empty()).then_some(trimmed)
        };
        info.error = None;
        let room_id = info.room_id.clone();
        let (change, cmd) = if prompt.knock {
            (
                Change::BanKnock,
                ManageCmd::BanKnock {
                    room_id,
                    user_id: prompt.user_id,
                    reason,
                },
            )
        } else {
            (
                Change::Ban,
                ManageCmd::Ban {
                    room_id,
                    user_id: prompt.user_id,
                    reason,
                },
            )
        };
        info.wait_for(Pending::Change(change), "Banning…");
        self.manage(cmd);
    }

    fn unban_member(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        info.ban_prompt = None;
        info.role_chooser = None;
        let Some(member) = info.selected_member().cloned() else {
            return;
        };
        if !member.unbannable {
            info.confirm = None;
            info.error = Some("You are not allowed to unban this user.".to_owned());
            return;
        }
        let user_id = member.user_id;
        if info.confirm != Some(Confirm::Unban(user_id.clone())) {
            info.confirm = Some(Confirm::Unban(user_id));
            return;
        }
        info.confirm = None;
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        info.wait_for(Pending::Change(Change::Unban), "Unbanning…");
        let room_id = info.room_id.clone();
        self.manage(ManageCmd::Unban {
            room_id,
            user_id,
            reason: None,
        });
    }

    fn open_role_chooser(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || info.editing.is_some() {
            return;
        }
        info.error = None;
        info.confirm = None;
        info.ban_prompt = None;
        let Some(member) = info.selected_member().cloned() else {
            return;
        };
        if !member.role_editable {
            info.error = Some("You are not allowed to change this member's role.".to_owned());
            return;
        }
        let Some(details) = info.details.as_ref() else {
            return;
        };
        let chooser = RoleChooser::new(
            &member,
            details.own_is_creator,
            details.own_power_level,
            details.admin_count,
        );
        if chooser.options.is_empty() {
            info.error = Some("No roles can be assigned with your power level.".to_owned());
            return;
        }
        info.role_chooser = Some(chooser);
    }

    fn pick_role_option(&mut self, index: usize) {
        if let Some(Modal::RoomInfo(info)) = &mut self.modal
            && let Some(chooser) = &mut info.role_chooser
        {
            if !chooser.select(index) {
                return;
            }
        } else {
            return;
        }
        self.submit_role_chooser();
    }

    fn submit_role_chooser(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        let Some(_details) = info.details.as_ref() else {
            return;
        };
        let Some(chooser) = info.role_chooser.as_mut() else {
            return;
        };
        let Some((level, _)) = chooser.chosen() else {
            return;
        };
        if level == chooser.current_level {
            info.role_chooser = None;
            return;
        }
        if chooser.needs_last_admin_confirm(level) && !chooser.confirm_last_admin {
            chooser.confirm_last_admin = true;
            return;
        }
        let user_id = chooser.user_id.clone();
        info.role_chooser = None;
        if !connected {
            info.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        info.error = None;
        info.wait_for(Pending::Change(Change::SetRole), "Updating role…");
        let room_id = info.room_id.clone();
        self.manage(ManageCmd::SetRole {
            room_id,
            user_id,
            level,
        });
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
            InfoRow::OwnName => {
                crate::text::sanitize_line(details.own_display_name.as_deref().unwrap_or(""))
                    == text
            }
            InfoRow::Topic
            | InfoRow::Favourite
            | InfoRow::LowPriority
            | InfoRow::Pinned
            | InfoRow::Notify
            | InfoRow::Invite
            | InfoRow::AddToSpace
            | InfoRow::RemoveFromSpace
            | InfoRow::Leave
            | InfoRow::KnockRequest(_)
            | InfoRow::AddChild
            | InfoRow::Child(_)
            | InfoRow::SettingsToggle
            | InfoRow::Setting(_)
            | InfoRow::Member(_) => crate::text::sanitize_line(&details.topic) == text,
        });
        if unchanged {
            info.editing = None;
            return;
        }
        let (command, change) = match room_text_change(row, room_id, text) {
            Ok(change) => change,
            Err(error) => {
                info.error = Some(error.to_owned());
                return;
            }
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

    fn finish_open_or_knock(
        &mut self,
        request: u64,
        result: Result<String, String>,
        fallback_toast: &str,
        open_toast: Option<&str>,
    ) {
        // What the preview said: a space is not in the room list once we are in it.
        let space = matches!(
            &self.modal,
            Some(Modal::New(popup)) if popup.request == Some(request)
                && popup.preview.as_ref().is_some_and(|preview| preview.space)
        );
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
                if space {
                    // Joined, it is selected once the list of spaces has it; asked
                    // to join, it waits in the room list as a request does.
                    self.manage.pending_space = Some((room_id.clone(), Instant::now()));
                    self.select_pending_space();
                }
                self.manage.pending_open = Some((room_id, Instant::now()));
                self.open_pending_room();
                if let Some(toast) = open_toast {
                    self.show_toast(toast);
                }
            }
            (Ok(_), false) => self.show_toast(fallback_toast),
            (Err(error), true) => match &mut self.modal {
                Some(Modal::New(popup)) => popup.error = Some(error),
                Some(Modal::RoomInfo(info)) => info.error = Some(error),
                _ => {}
            },
            (Err(error), false) => self.show_toast(error),
        }
    }

    pub(super) fn on_manage(&mut self, event: ManageEvent) {
        match event {
            ManageEvent::Opened { request, result } => {
                if self.on_user_info_manage_opened(request, &result) {
                    return;
                }
                self.finish_open_or_knock(
                    request,
                    result,
                    "Done: the room is in your room list",
                    None,
                );
            }
            ManageEvent::Knocked { request, result } => {
                self.finish_open_or_knock(
                    request,
                    result,
                    "Join request sent",
                    Some("Join request sent"),
                );
            }
            ManageEvent::Preview { request, result } => {
                let mut membership_override = None;
                if let Ok(ref preview) = result
                    && preview.membership == PreviewMembership::None
                {
                    // A space we are in is among the spaces, not the rooms.
                    let space = self.spaces.iter().any(|space| space.id == preview.room_id);
                    membership_override = self
                        .rooms
                        .iter()
                        .find(|r| r.id == preview.room_id)
                        .map(|r| match r.kind {
                            RoomKind::Invite => PreviewMembership::Invited,
                            RoomKind::Knock => PreviewMembership::Knocked,
                            RoomKind::Favourite
                            | RoomKind::Person
                            | RoomKind::Room
                            | RoomKind::LowPriority => PreviewMembership::Joined,
                        })
                        .or(space.then_some(PreviewMembership::Joined));
                }
                if let Some(Modal::New(popup)) = &mut self.modal
                    && popup.stage == Stage::Preview
                    && popup.preview_request == Some(request)
                {
                    popup.preview_loading = false;
                    popup.preview_request = None;
                    match result {
                        Ok(mut preview) => {
                            if let Some(m) = membership_override {
                                preview.membership = m;
                            }
                            popup.preview = Some(*preview);
                            popup.error = None;
                            popup.focus = popup.default_preview_focus();
                        }
                        Err(error) => {
                            popup.preview = None;
                            popup.error = Some(error);
                            popup.focus = popup.default_preview_focus();
                        }
                    }
                }
            }
            ManageEvent::NoDirectChat { request, user_id } => {
                if self.on_user_info_no_direct_chat(request, &user_id) {
                    return;
                }
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
            ManageEvent::SpaceCreated { request, result } => {
                self.on_space_created(request, result);
            }
            ManageEvent::SpaceEdited {
                request,
                space_id,
                child_id,
                edit,
                result,
            } => self.on_space_edited(request, &space_id, &child_id, edit, result),
            ManageEvent::Gone { room_id } => self.info_gone(&room_id),
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

    pub(super) fn on_change_done(
        &mut self,
        room_id: &str,
        change: Change,
        result: Result<(), String>,
    ) {
        let done = match change {
            Change::Invite => "Invitation sent",
            Change::Kick => "Removed from the room",
            Change::Ban => "Banned from the room",
            Change::Unban => "Unbanned",
            Change::SetRole => "Role updated",
            Change::AcceptKnock => "Join request accepted",
            Change::DeclineKnock => "Join request declined",
            Change::BanKnock => "Declined and banned",
            Change::Name => "Room renamed",
            Change::OwnName => "Name in room updated",
            Change::Topic => "Topic changed",
            Change::Favourite(true) => "Added to favourites",
            Change::Favourite(false) => "Removed from favourites",
            Change::LowPriority(true) => "Marked low priority",
            Change::LowPriority(false) => "Removed from low priority",
            Change::Notify => "",
            Change::JoinRule => "Who can join updated",
            Change::HistoryVisibility => "History visibility updated",
            Change::EnableEncryption => "Encryption turned on",
            Change::RoomAvatar => "Room picture updated",
            Change::RemoveRoomAvatar => "Room picture removed",
            Change::Addresses => "Room addresses updated",
            Change::CreateLocalAlias => "Local address created",
            Change::DeleteLocalAlias => "Local address deleted",
            Change::Direct(true) => "Marked as a direct chat",
            Change::Direct(false) => "Unmarked as a direct chat",
            Change::Permissions => "Permissions updated",
        };
        if self.on_user_info_manage_done(room_id, change, done, &result) {
            return;
        }
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
                        match change {
                            Change::Addresses
                            | Change::CreateLocalAlias
                            | Change::DeleteLocalAlias => {
                                if let Some(SettingSubview::Addresses {
                                    input,
                                    confirm_delete,
                                    error,
                                    notice,
                                    ..
                                }) = &mut info.setting_view
                                {
                                    if change != Change::DeleteLocalAlias {
                                        input.clear();
                                    }
                                    *confirm_delete = None;
                                    *error = None;
                                    *notice = (!done.is_empty()).then(|| done.to_owned());
                                }
                            }
                            Change::JoinRule
                            | Change::HistoryVisibility
                            | Change::EnableEncryption
                            | Change::RoomAvatar
                            | Change::RemoveRoomAvatar
                            | Change::Permissions => {
                                info.setting_view = None;
                                if !done.is_empty() {
                                    info.notice = Some(done.to_owned());
                                }
                            }
                            Change::Direct(_) if !done.is_empty() => {
                                info.notice = Some(done.to_owned());
                            }
                            Change::Invite
                            | Change::Kick
                            | Change::Ban
                            | Change::Unban
                            | Change::SetRole
                            | Change::AcceptKnock
                            | Change::DeclineKnock
                            | Change::BanKnock
                            | Change::Name
                            | Change::OwnName
                            | Change::Topic
                            | Change::Favourite(_)
                            | Change::LowPriority(_)
                            | Change::Notify
                            | Change::Direct(_) => {}
                        }
                        if !done.is_empty() {
                            self.show_toast(done);
                        }
                    }
                    Err(error) => {
                        self.setting_view_error(error);
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
            // A space is not in the room list; `follow_spaces` keeps its info.
            Some(Modal::RoomInfo(info)) if info.space => return,
            Some(Modal::RoomInfo(info)) => {
                (info.room_id.clone(), info.pending == Some(Pending::Leave))
            }
            // Spaces are not in the room list.
            Some(Modal::New(popup)) if popup.stage == Stage::Invite && popup.into_space => return,
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

    /// The worker says that we are no longer in the room whose info is open: the info
    /// closes, as when the room leaves the room list. Our own leaving closes it when
    /// its answer is there.
    fn info_gone(&mut self, room_id: &str) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if info.room_id != room_id || info.pending == Some(Pending::Leave) {
            return;
        }
        let what = if info.space { "space" } else { "room" };
        self.close_manage();
        self.show_toast(format!("You are no longer in that {what}"));
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
        if Some(index) != self.open_index() || self.thread_open() {
            self.open_room(index);
        }
        if let Some(at) = self.visible_rooms().iter().position(|i| *i == index) {
            self.sidebar.cursor = at;
            self.sidebar.reveal_cursor = true;
        }
    }

    fn send_room_setting(&mut self, change: Change, cmd: ManageCmd, busy: &'static str) -> bool {
        if self.matrix.is_none() {
            self.setting_view_error(NO_ACCOUNT);
            return false;
        }
        if let Some(Modal::RoomInfo(info)) = &mut self.modal {
            info.error = None;
            info.notice = None;
            info.wait_for(Pending::Change(change), busy);
        }
        self.manage(cmd)
    }

    fn setting_view_error(&mut self, message: impl Into<String>) {
        let message = message.into();
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        match &mut info.setting_view {
            Some(SettingSubview::EncryptConfirm { error, .. }) => *error = Some(message),
            Some(SettingSubview::Picture { error, notice, .. }) => {
                *error = Some(message);
                *notice = None;
            }
            Some(SettingSubview::Addresses { error, notice, .. }) => {
                *error = Some(message);
                *notice = None;
            }
            Some(SettingSubview::Permissions { error, .. }) => *error = Some(message),
            _ => info.error = Some(message),
        }
    }

    fn activate_room_setting(&mut self, setting: RoomSetting) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        let Some(details) = &info.details else {
            return;
        };
        let editable = info.setting_editable(setting);
        match setting {
            RoomSetting::JoinRule => {
                if !editable {
                    info.error = Some(
                        if details.join_rule.is_some_and(|r| r.to_ruma().is_none()) {
                            "This room uses a restricted or custom join rule that cannot be edited here.".to_owned()
                        } else {
                            "You do not have permission to change who can join.".to_owned()
                        },
                    );
                    return;
                }
                let selected = details
                    .join_rule
                    .and_then(|cur| RoomJoinRule::EDITABLE.iter().position(|r| *r == cur))
                    .unwrap_or(0);
                info.setting_view = Some(SettingSubview::JoinRule { selected });
            }
            RoomSetting::History => {
                if !editable {
                    info.error =
                        Some("You do not have permission to change history visibility.".to_owned());
                    return;
                }
                let selected = details
                    .history_visibility
                    .and_then(|cur| RoomHistory::ALL.iter().position(|h| *h == cur))
                    .unwrap_or(0);
                info.setting_view = Some(SettingSubview::History { selected });
            }
            RoomSetting::Encryption => {
                if details.encrypted == Some(true) {
                    info.error =
                        Some("End-to-end encryption cannot be turned off once enabled.".to_owned());
                    return;
                }
                if details.encrypted.is_none() {
                    info.error = Some("Encryption status is not known yet.".to_owned());
                    return;
                }
                if !editable {
                    info.error =
                        Some("You do not have permission to turn on encryption.".to_owned());
                    return;
                }
                info.setting_view = Some(SettingSubview::EncryptConfirm {
                    confirm: LineEdit::new().with_max_bytes(16),
                    focus: 0,
                    error: None,
                });
            }
            RoomSetting::Picture => {
                if !editable {
                    info.error =
                        Some("You do not have permission to change the room picture.".to_owned());
                    return;
                }
                info.setting_view = Some(SettingSubview::Picture {
                    path: LineEdit::new().with_max_bytes(FIELD_MAX),
                    focus: 0,
                    error: None,
                    notice: None,
                });
            }
            RoomSetting::Addresses => {
                info.setting_view = Some(SettingSubview::Addresses {
                    input: LineEdit::new().with_max_bytes(FIELD_MAX),
                    focus: 0,
                    selected: 0,
                    confirm_delete: None,
                    error: None,
                    notice: None,
                });
            }
            RoomSetting::Permissions => {
                info.setting_view = Some(SettingSubview::Permissions {
                    draft: details.permissions,
                    selected: 0,
                    error: None,
                });
            }
            RoomSetting::Direct => {}
        }
    }

    fn on_setting_view_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() {
            return;
        }
        let can_edit_addresses = info.setting_editable(RoomSetting::Addresses);
        let can_edit_permissions = info.setting_editable(RoomSetting::Permissions);
        let has_avatar = info
            .details
            .as_ref()
            .is_some_and(|d| d.avatar_url.is_some());
        let has_canonical = info
            .details
            .as_ref()
            .is_some_and(|d| d.canonical_alias.is_some());
        let entry_count = info
            .details
            .as_ref()
            .map_or(0, |d| address_entries(d).len());
        match &mut info.setting_view {
            Some(SettingSubview::JoinRule { selected }) => {
                let len = RoomJoinRule::EDITABLE.len() as isize;
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "foreign crossterm::event::KeyCode"
                )]
                match code {
                    KeyCode::Up | KeyCode::Char('k') if code == KeyCode::Up || plain => {
                        *selected = (*selected as isize - 1).rem_euclid(len) as usize;
                    }
                    KeyCode::Down | KeyCode::Char('j') if code == KeyCode::Down || plain => {
                        *selected = (*selected as isize + 1).rem_euclid(len) as usize;
                    }
                    KeyCode::Enter if plain => {
                        let choice = *selected;
                        self.on_setting_choice(choice);
                    }
                    _ => {}
                }
            }
            Some(SettingSubview::History { selected }) => {
                let len = RoomHistory::ALL.len() as isize;
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "foreign crossterm::event::KeyCode"
                )]
                match code {
                    KeyCode::Up | KeyCode::Char('k') if code == KeyCode::Up || plain => {
                        *selected = (*selected as isize - 1).rem_euclid(len) as usize;
                    }
                    KeyCode::Down | KeyCode::Char('j') if code == KeyCode::Down || plain => {
                        *selected = (*selected as isize + 1).rem_euclid(len) as usize;
                    }
                    KeyCode::Enter if plain => {
                        let choice = *selected;
                        self.on_setting_choice(choice);
                    }
                    _ => {}
                }
            }
            Some(SettingSubview::EncryptConfirm {
                confirm,
                focus,
                error,
            }) =>
            {
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "foreign crossterm::event::KeyCode"
                )]
                match code {
                    KeyCode::Tab if mods.is_empty() => {
                        *focus = step_focus(*focus, &[0, 1, 2], 1);
                    }
                    KeyCode::BackTab => {
                        *focus = step_focus(*focus, &[0, 1, 2], -1);
                    }
                    KeyCode::Left | KeyCode::Right if *focus >= 1 && plain => {
                        let step = if code == KeyCode::Left { -1 } else { 1 };
                        *focus = step_focus(*focus, &[1, 2], step);
                    }
                    KeyCode::Enter if plain => {
                        if *focus == 2 {
                            info.setting_view = None;
                        } else {
                            self.submit_enable_encryption();
                        }
                    }
                    _ if *focus == 0
                        && confirm.handle_key(code, mods, EditKeys::FULL).changed() =>
                    {
                        *error = None;
                    }
                    _ => {}
                }
            }
            Some(SettingSubview::Picture {
                path,
                focus,
                error,
                notice,
            }) => {
                let stops: &[usize] = if has_avatar {
                    &[0, 1, 2, 3]
                } else {
                    &[0, 1, 3]
                };
                let btn_stops: &[usize] = if has_avatar { &[1, 2, 3] } else { &[1, 3] };
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "foreign crossterm::event::KeyCode"
                )]
                match code {
                    KeyCode::Tab if mods.is_empty() && *focus == 0 && !path.is_empty() => {
                        self.complete_room_avatar_path();
                    }
                    KeyCode::Tab if mods.is_empty() => {
                        *focus = step_focus(*focus, stops, 1);
                    }
                    KeyCode::BackTab => {
                        *focus = step_focus(*focus, stops, -1);
                    }
                    KeyCode::Left | KeyCode::Right if *focus >= 1 && plain => {
                        let step = if code == KeyCode::Left { -1 } else { 1 };
                        *focus = step_focus(*focus, btn_stops, step);
                    }
                    KeyCode::Enter if plain => {
                        let slot = if *focus == 0 { 1 } else { *focus };
                        self.on_setting_slot(slot);
                    }
                    _ if *focus == 0 && path.handle_key(code, mods, EditKeys::FULL).changed() => {
                        *error = None;
                        *notice = None;
                    }
                    _ => {}
                }
            }
            Some(SettingSubview::Addresses {
                input,
                focus,
                selected,
                confirm_delete,
                error,
                notice,
            }) => {
                if !can_edit_addresses {
                    #[expect(
                        clippy::wildcard_enum_match_arm,
                        reason = "foreign crossterm::event::KeyCode"
                    )]
                    match code {
                        KeyCode::Up | KeyCode::Char('k') if code == KeyCode::Up || plain => {
                            *selected = selected.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j')
                            if (code == KeyCode::Down || plain) && *selected + 1 < entry_count =>
                        {
                            *selected += 1;
                        }
                        _ => {}
                    }
                    return;
                }
                let stops: &[usize] = if has_canonical {
                    &[0, 1, 2, 3]
                } else {
                    &[0, 1, 2]
                };
                let btn_stops: &[usize] = if has_canonical { &[1, 2, 3] } else { &[1, 2] };
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "foreign crossterm::event::KeyCode"
                )]
                match code {
                    KeyCode::Tab if mods.is_empty() => {
                        *focus = step_focus(*focus, stops, 1);
                    }
                    KeyCode::BackTab => {
                        *focus = step_focus(*focus, stops, -1);
                    }
                    KeyCode::Up => {
                        *selected = selected.saturating_sub(1);
                        *confirm_delete = None;
                    }
                    KeyCode::Down => {
                        if *selected + 1 < entry_count {
                            *selected += 1;
                        }
                        *confirm_delete = None;
                    }
                    KeyCode::Left | KeyCode::Right if *focus >= 1 && plain => {
                        let step = if code == KeyCode::Left { -1 } else { 1 };
                        *focus = step_focus(*focus, btn_stops, step);
                    }
                    KeyCode::Enter if plain => {
                        let slot = if *focus == 0 { 1 } else { *focus };
                        self.on_setting_slot(slot);
                    }
                    KeyCode::Char('k') if *focus != 0 && plain => {
                        *selected = selected.saturating_sub(1);
                        *confirm_delete = None;
                    }
                    KeyCode::Char('j') if *focus != 0 && plain => {
                        if *selected + 1 < entry_count {
                            *selected += 1;
                        }
                        *confirm_delete = None;
                    }
                    KeyCode::Char('m') if *focus != 0 && plain => {
                        let idx = *selected;
                        self.make_main_alias_at(idx);
                    }
                    KeyCode::Char('u') if *focus != 0 && plain => {
                        let idx = *selected;
                        self.unpublish_alias_at(idx);
                    }
                    KeyCode::Delete if *focus != 0 && plain => {
                        let idx = *selected;
                        self.delete_local_alias_at(idx);
                    }
                    _ if *focus == 0 && input.handle_key(code, mods, EditKeys::FULL).changed() => {
                        *confirm_delete = None;
                        *error = None;
                        *notice = None;
                    }
                    _ => {}
                }
            }
            Some(SettingSubview::Permissions {
                draft,
                selected,
                error,
            }) => {
                if !can_edit_permissions {
                    if code == KeyCode::Enter && plain {
                        info.setting_view = None;
                    }
                    return;
                }
                let rows = PermAction::ALL.len();
                let max_sel = rows + 1;
                #[expect(
                    clippy::wildcard_enum_match_arm,
                    reason = "foreign crossterm::event::KeyCode"
                )]
                match code {
                    KeyCode::Up | KeyCode::Char('k') if code == KeyCode::Up || plain => {
                        *selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') if code == KeyCode::Down || plain => {
                        if *selected < max_sel {
                            *selected += 1;
                        }
                    }
                    KeyCode::Tab if mods.is_empty() => {
                        *selected = if *selected < rows {
                            rows
                        } else if *selected == rows {
                            rows + 1
                        } else {
                            0
                        };
                    }
                    KeyCode::BackTab => {
                        *selected = if *selected == rows + 1 {
                            rows
                        } else if *selected == rows {
                            rows.saturating_sub(1)
                        } else if *selected == 0 {
                            rows + 1
                        } else {
                            *selected - 1
                        };
                    }
                    KeyCode::Left | KeyCode::Right if plain => {
                        let step = if code == KeyCode::Left { -1 } else { 1 };
                        if let Some(action) = PermAction::ALL.get(*selected).copied() {
                            let next = step_perm_level(action.get(draft), step);
                            action.set(draft, next);
                            *error = None;
                        } else if *selected >= rows {
                            *selected = if *selected == rows { rows + 1 } else { rows };
                        }
                    }
                    KeyCode::Enter if plain => {
                        if *selected == max_sel {
                            info.setting_view = None;
                        } else {
                            self.submit_permissions();
                        }
                    }
                    _ => {}
                }
            }
            None => {}
        }
    }

    fn on_setting_choice(&mut self, idx: usize) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() {
            return;
        }
        let room_id = info.room_id.clone();
        match &mut info.setting_view {
            Some(SettingSubview::JoinRule { selected }) => {
                let Some(&rule) = RoomJoinRule::EDITABLE.get(idx) else {
                    return;
                };
                *selected = idx;
                if info.details.as_ref().and_then(|d| d.join_rule) == Some(rule) {
                    info.setting_view = None;
                    return;
                }
                self.send_room_setting(
                    Change::JoinRule,
                    ManageCmd::SetJoinRule { room_id, rule },
                    "Saving…",
                );
            }
            Some(SettingSubview::History { selected }) => {
                let Some(&visibility) = RoomHistory::ALL.get(idx) else {
                    return;
                };
                *selected = idx;
                if info.details.as_ref().and_then(|d| d.history_visibility) == Some(visibility) {
                    info.setting_view = None;
                    return;
                }
                self.send_room_setting(
                    Change::HistoryVisibility,
                    ManageCmd::SetHistoryVisibility {
                        room_id,
                        visibility,
                    },
                    "Saving…",
                );
            }
            _ => {}
        }
    }

    fn on_setting_slot(&mut self, slot: usize) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() {
            return;
        }
        match &info.setting_view {
            Some(SettingSubview::EncryptConfirm { .. }) => match slot {
                1 => self.submit_enable_encryption(),
                2 => info.setting_view = None,
                _ => {}
            },
            Some(SettingSubview::Picture { .. }) => match slot {
                1 => self.submit_room_avatar(),
                2 => self.submit_remove_room_avatar(),
                3 => info.setting_view = None,
                _ => {}
            },
            Some(SettingSubview::Addresses { .. }) => match slot {
                1 => self.create_local_alias(),
                2 => self.publish_input_alt_alias(),
                3 => self.clear_main_alias(),
                _ => {}
            },
            Some(SettingSubview::Permissions { .. }) => match slot {
                0 => self.submit_permissions(),
                1 => info.setting_view = None,
                _ => {}
            },
            _ => {}
        }
    }

    fn submit_enable_encryption(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if !info.setting_editable(RoomSetting::Encryption) {
            self.setting_view_error("Encryption cannot be turned on in this room.");
            return;
        }
        let room_id = info.room_id.clone();
        let typed = match &info.setting_view {
            Some(SettingSubview::EncryptConfirm { confirm, .. }) => {
                confirm.as_str().trim().to_owned()
            }
            _ => return,
        };
        if typed != "ENCRYPT" {
            self.setting_view_error("Type ENCRYPT in capitals to turn encryption on.");
            return;
        }
        self.send_room_setting(
            Change::EnableEncryption,
            ManageCmd::EnableEncryption { room_id },
            "Turning encryption on…",
        );
    }

    fn complete_room_avatar_path(&mut self) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        let Some(SettingSubview::Picture {
            path,
            error,
            notice,
            ..
        }) = &mut info.setting_view
        else {
            return;
        };
        match complete_path_field(path) {
            PathCompleteResult::Empty => {
                *error = Some("Type the beginning of a file path first.".to_owned());
                *notice = None;
            }
            PathCompleteResult::Handled {
                notice: msg,
                changed,
                has_options,
            } => {
                if changed || has_options {
                    *error = None;
                    *notice = msg;
                } else {
                    *error = Some("No file or folder starts with that path.".to_owned());
                    *notice = None;
                }
            }
            PathCompleteResult::NoMatch => {
                *error = Some("No file or folder starts with that path.".to_owned());
                *notice = None;
            }
        }
    }

    fn submit_room_avatar(&mut self) {
        let limit = self.attach.limit;
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if !info.setting_editable(RoomSetting::Picture) {
            self.setting_view_error("You do not have permission to change the room picture.");
            return;
        }
        let room_id = info.room_id.clone();
        let raw = match &info.setting_view {
            Some(SettingSubview::Picture { path, .. }) => path.as_str().to_owned(),
            _ => return,
        };
        let path = match validate_avatar_path(&raw, limit) {
            Ok(path) => path,
            Err(err) => {
                self.setting_view_error(err);
                return;
            }
        };
        self.send_room_setting(
            Change::RoomAvatar,
            ManageCmd::SetRoomAvatar { room_id, path },
            "Uploading picture…",
        );
    }

    fn submit_remove_room_avatar(&mut self) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if !info.setting_editable(RoomSetting::Picture) {
            self.setting_view_error("You do not have permission to change the room picture.");
            return;
        }
        if info.details.as_ref().is_none_or(|d| d.avatar_url.is_none()) {
            self.setting_view_error("This room has no picture to remove.");
            return;
        }
        let room_id = info.room_id.clone();
        self.send_room_setting(
            Change::RemoveRoomAvatar,
            ManageCmd::RemoveRoomAvatar { room_id },
            "Removing picture…",
        );
    }

    fn normalized_address_input(&mut self) -> Option<NormalizedAddressInput> {
        let (room_id, raw, own_hs, canonical, alts) = {
            let Some(Modal::RoomInfo(info)) = &self.modal else {
                return None;
            };
            let Some(details) = &info.details else {
                return None;
            };
            let Some(SettingSubview::Addresses { input, .. }) = &info.setting_view else {
                return None;
            };
            (
                info.room_id.clone(),
                input.as_str().to_owned(),
                details.own_homeserver.clone(),
                details.canonical_alias.clone(),
                details.alt_aliases.clone(),
            )
        };
        match normalize_alias_input(&raw, own_hs.as_deref()) {
            Ok(alias) => Some(NormalizedAddressInput {
                room_id,
                alias,
                canonical,
                alts,
                own_hs,
            }),
            Err(err) => {
                self.setting_view_error(err);
                None
            }
        }
    }

    fn submit_addresses_update(&mut self, canonical: Option<String>, alt_aliases: Vec<String>) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if !info.setting_editable(RoomSetting::Addresses) {
            self.setting_view_error("You do not have permission to edit room addresses.");
            return;
        }
        let room_id = info.room_id.clone();
        self.send_room_setting(
            Change::Addresses,
            ManageCmd::SetAddresses {
                room_id,
                canonical,
                alt_aliases,
            },
            "Saving addresses…",
        );
    }

    fn create_local_alias(&mut self) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if !info.setting_editable(RoomSetting::Addresses) {
            self.setting_view_error("You do not have permission to edit room addresses.");
            return;
        }
        let Some(parsed) = self.normalized_address_input() else {
            return;
        };
        if let Some(hs) = parsed.own_hs.as_deref()
            && !parsed.alias.ends_with(&format!(":{hs}"))
        {
            self.setting_view_error(format!(
                "Local addresses must be on your homeserver (:{hs})."
            ));
            return;
        }
        self.send_room_setting(
            Change::CreateLocalAlias,
            ManageCmd::CreateLocalAlias {
                room_id: parsed.room_id,
                alias: parsed.alias,
            },
            "Creating local address…",
        );
    }

    fn publish_input_alt_alias(&mut self) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if !info.setting_editable(RoomSetting::Addresses) {
            self.setting_view_error("You do not have permission to edit room addresses.");
            return;
        }
        let Some(mut parsed) = self.normalized_address_input() else {
            return;
        };
        if parsed.canonical.as_deref() == Some(parsed.alias.as_str()) {
            self.setting_view_error("That address is already the main address.");
            return;
        }
        if parsed.alts.contains(&parsed.alias) {
            self.setting_view_error("That address is already published as an alternate.");
            return;
        }
        parsed.alts.push(parsed.alias);
        self.submit_addresses_update(parsed.canonical, parsed.alts);
    }

    fn clear_main_alias(&mut self) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if !info.setting_editable(RoomSetting::Addresses) {
            self.setting_view_error("You do not have permission to edit room addresses.");
            return;
        }
        let Some(details) = &info.details else {
            return;
        };
        if details.canonical_alias.is_none() {
            return;
        }
        let alts = details.alt_aliases.clone();
        self.submit_addresses_update(None, alts);
    }

    fn make_main_alias_at(&mut self, idx: usize) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if info.busy.is_some() || !info.setting_editable(RoomSetting::Addresses) {
            return;
        }
        let Some(details) = &info.details else {
            return;
        };
        let entries = address_entries(details);
        let Some(entry) = entries.get(idx) else {
            return;
        };
        if entry.is_canonical {
            return;
        }
        let next_main = entry.alias.clone();
        let mut alts: Vec<String> = details
            .alt_aliases
            .iter()
            .filter(|a| *a != &next_main)
            .cloned()
            .collect();
        if let Some(prev) = &details.canonical_alias
            && prev != &next_main
            && !alts.contains(prev)
        {
            alts.push(prev.clone());
        }
        self.submit_addresses_update(Some(next_main), alts);
    }

    fn publish_alt_alias_at(&mut self, idx: usize) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if info.busy.is_some() || !info.setting_editable(RoomSetting::Addresses) {
            return;
        }
        let Some(details) = &info.details else {
            return;
        };
        let entries = address_entries(details);
        let Some(entry) = entries.get(idx) else {
            return;
        };
        if entry.is_canonical || entry.is_alt {
            return;
        }
        let canonical = details.canonical_alias.clone();
        let mut alts = details.alt_aliases.clone();
        alts.push(entry.alias.clone());
        self.submit_addresses_update(canonical, alts);
    }

    fn unpublish_alias_at(&mut self, idx: usize) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        if info.busy.is_some() || !info.setting_editable(RoomSetting::Addresses) {
            return;
        }
        let Some(details) = &info.details else {
            return;
        };
        let entries = address_entries(details);
        let Some(entry) = entries.get(idx) else {
            return;
        };
        if !entry.is_canonical && !entry.is_alt {
            return;
        }
        let canonical = details
            .canonical_alias
            .clone()
            .filter(|a| a != &entry.alias);
        let alts = details
            .alt_aliases
            .iter()
            .filter(|a| *a != &entry.alias)
            .cloned()
            .collect();
        self.submit_addresses_update(canonical, alts);
    }

    fn delete_local_alias_at(&mut self, idx: usize) {
        let Some(Modal::RoomInfo(info)) = &mut self.modal else {
            return;
        };
        if info.busy.is_some() || !info.setting_editable(RoomSetting::Addresses) {
            return;
        }
        let room_id = info.room_id.clone();
        let Some(details) = &info.details else {
            return;
        };
        let entries = address_entries(details);
        let Some(entry) = entries.get(idx) else {
            return;
        };
        let own_hs = details.own_homeserver.as_deref();
        let local_on_own_hs =
            entry.is_local && own_hs.is_none_or(|hs| entry.alias.ends_with(&format!(":{hs}")));
        if !local_on_own_hs {
            self.setting_view_error("Only local addresses on your homeserver can be deleted here.");
            return;
        }
        let alias = entry.alias.clone();
        if let Some(SettingSubview::Addresses {
            confirm_delete,
            error,
            notice,
            ..
        }) = &mut info.setting_view
        {
            if confirm_delete.as_deref() != Some(alias.as_str()) {
                *confirm_delete = Some(alias);
                *error = None;
                *notice = None;
                return;
            }
            *confirm_delete = None;
        } else {
            return;
        }
        self.send_room_setting(
            Change::DeleteLocalAlias,
            ManageCmd::DeleteLocalAlias { room_id, alias },
            "Deleting local address…",
        );
    }

    fn submit_permissions(&mut self) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        let Some(details) = &info.details else {
            return;
        };
        let Some(SettingSubview::Permissions { draft, .. }) = &info.setting_view else {
            return;
        };
        if let Err(err) = validate_permissions_draft(details, draft) {
            self.setting_view_error(err);
            return;
        }
        let room_id = info.room_id.clone();
        let permissions = *draft;
        self.send_room_setting(
            Change::Permissions,
            ManageCmd::SetPermissions {
                room_id,
                permissions,
            },
            "Saving permissions…",
        );
    }
}

struct NormalizedAddressInput {
    room_id: String,
    alias: String,
    canonical: Option<String>,
    alts: Vec<String>,
    own_hs: Option<String>,
}

/// What setting the room's name (`InfoRow::Name`), its topic (`InfoRow::Topic`) or
/// one's own name in it (`InfoRow::OwnName`) to `text` sends, after the checks every
/// way of doing so shares. `text` has been through `clean_input` already.
pub(super) fn room_text_change(
    row: InfoRow,
    room_id: String,
    text: String,
) -> Result<(ManageCmd, Change), &'static str> {
    match row {
        InfoRow::Name if text.is_empty() => Err("A room needs a name."),
        InfoRow::Name | InfoRow::OwnName if text.len() > NAME_MAX => Err("That name is too long."),
        _ if text.len() > FIELD_MAX => Err("That is too long."),
        InfoRow::Name => Ok((
            ManageCmd::SetName {
                room_id,
                name: text,
            },
            Change::Name,
        )),
        InfoRow::OwnName => Ok((
            ManageCmd::SetOwnName {
                room_id,
                name: (!text.is_empty()).then_some(text),
            },
            Change::OwnName,
        )),
        InfoRow::Topic
        | InfoRow::Favourite
        | InfoRow::LowPriority
        | InfoRow::Pinned
        | InfoRow::Notify
        | InfoRow::Invite
        | InfoRow::AddToSpace
        | InfoRow::RemoveFromSpace
        | InfoRow::Leave
        | InfoRow::KnockRequest(_)
        | InfoRow::AddChild
        | InfoRow::Child(_)
        | InfoRow::SettingsToggle
        | InfoRow::Setting(_)
        | InfoRow::Member(_) => Ok((
            ManageCmd::SetTopic {
                room_id,
                topic: text,
            },
            Change::Topic,
        )),
    }
}

fn step_focus(current: usize, stops: &[usize], step: isize) -> usize {
    let idx = stops.iter().position(|&s| s == current).unwrap_or(0) as isize;
    stops[(idx + step).rem_euclid(stops.len() as isize) as usize]
}

fn step_perm_level(current: i64, step: isize) -> i64 {
    let levels = PermAction::LEVELS;
    let idx = levels
        .iter()
        .position(|(level, _)| *level >= current)
        .unwrap_or(levels.len() - 1) as isize;
    let next_idx = (idx + step).clamp(0, levels.len() as isize - 1) as usize;
    levels[next_idx].0
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
            power_level: if members.is_empty() { 100 } else { 0 },
            invited: false,
            own: message.own,
            removable: false,
            ..RoomMember::default()
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
        low_priority: room.kind == RoomKind::LowPriority,
        notify: Some(NotifyLevel::All),
        can_rename: false,
        can_set_topic: false,
        can_invite: false,
        members,
        ..RoomDetails::default()
    }
}

fn display_user_label(name: &str, user_id: &str) -> String {
    if name.is_empty() {
        user_id.to_owned()
    } else {
        sanitize_line(name)
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
            encrypted: false,
            id: id.into(),
            name: name.into(),
            category,
            notifications: 0,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            preview: None,
            avatar: None,
            alias: None,
            direct: category == RoomCategory::Person,
            muted: false,
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
            power_level: match role {
                MemberRole::Admin | MemberRole::Owner => 100,
                MemberRole::Moderator => 50,
                MemberRole::User => 0,
            },
            invited,
            own: name == "me",
            removable: name != "me" && role < MemberRole::Admin,
            ..RoomMember::default()
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
    fn keys_typed_into_a_member_filter_that_nobody_matches_never_change_the_room() {
        let (mut app, _terminal, mut commands) = connected();
        with_info(&mut app, &mut commands);
        let select = |app: &mut App, row: InfoRow| {
            if let Some(Modal::RoomInfo(info)) = &mut app.modal {
                info.settings_open = true;
                info.select(row);
            }
            assert_eq!(info(app).current(), Some(row));
        };
        let direct = InfoRow::Setting(RoomSetting::Direct);
        // Nobody is called "zed": the selection stays on the setting, and the space
        // is part of the filter.
        select(&mut app, direct);
        type_text(&mut app, "zed q");
        assert_eq!(info(&app).filter.as_str(), "zed q");
        assert_eq!(info(&app).current(), Some(direct));
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));
        assert!(info(&app).filter.is_empty());
        // So on the line of the settings, where the arrows move in the filter too, as
        // they do on the notification level.
        for row in [InfoRow::SettingsToggle, InfoRow::Notify] {
            select(&mut app, row);
            type_text(&mut app, "zz z");
            app.on_event(key(KeyCode::Left));
            app.on_event(key(KeyCode::Right));
            assert_eq!(info(&app).filter.as_str(), "zz z");
            assert!(info(&app).settings_open);
            assert!(sent(&mut commands).is_empty(), "{row:?}");
            app.on_event(key(KeyCode::Esc));
        }
        // A member's row takes Del only without Ctrl or Alt.
        select(&mut app, InfoRow::Member(2));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Delete,
            KeyModifiers::CONTROL,
        )));
        assert!(info(&app).confirm.is_none());
        assert!(sent(&mut commands).is_empty());

        // Enter does not run a row that is out of view: not Leave, not a setting.
        for row in [InfoRow::Leave, direct, InfoRow::Favourite] {
            select(&mut app, row);
            type_text(&mut app, "zed");
            assert!(info(&app).typing_blind());
            app.on_event(key(KeyCode::Enter));
            assert!(info(&app).confirm.is_none(), "{row:?}");
            assert_eq!(info(&app).filter.as_str(), "zed");
            assert!(sent(&mut commands).is_empty(), "{row:?}");
            app.on_event(key(KeyCode::Esc));
        }
        // Up from the first row while it is out of view brings it into view.
        if let Some(Modal::RoomInfo(info)) = &mut app.modal {
            info.selected = 0;
        }
        type_text(&mut app, "zed");
        assert!(info(&app).typing_blind());
        app.on_event(key(KeyCode::Up));
        assert!(!info(&app).typing_blind());
        assert!(info(&app).reveal && info(&app).selected == 0);
        app.on_event(key(KeyCode::Esc));

        // Chosen with the arrows, a row is in view again, however low the window.
        let mut short = Terminal::new(TestBackend::new(100, 18)).unwrap();
        select(&mut app, direct);
        type_text(&mut app, "zed");
        assert!(info(&app).typing_blind());
        find(&screen(&mut app, &mut short), "⌕ zed");
        for _ in 0..20 {
            if info(&app).current() == Some(InfoRow::Leave) {
                break;
            }
            app.on_event(key(KeyCode::Up));
        }
        assert!(!info(&app).typing_blind());
        find(&screen(&mut app, &mut short), "Leave room");
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
            [ManageCmd::Kick { room_id, user_id, reason: None }]
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
        // Self has no direct-chat button; Enter on another member opens User Info
        // while " chat " starts a direct chat immediately.
        assert!(!screen(&mut app, &mut terminal).contains(" chat "));
        app.on_event(ctrl('u'));
        type_text(&mut app, "carol");
        click_on(&mut app, &mut terminal, " chat ");
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
        // Low priority is mutually exclusive with favourite.
        app.on_event(key(KeyCode::Down));
        assert_eq!(info(&app).current(), Some(InfoRow::LowPriority));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetLowPriority { room_id, on: true }] if room_id == ROOM
        ));
        {
            let d = info(&app).details.as_ref().unwrap();
            assert!(d.low_priority && !d.favourite);
        }
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::LowPriority(true),
                result: Ok(()),
            },
        );
        // Notifications (after Pinned messages): arrows step through the three modes.
        app.on_event(key(KeyCode::Down));
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
    fn the_room_info_shows_the_pinned_messages_of_its_own_room_with_no_chat_open() {
        let mut app = App::demo();
        for _ in 0..3 {
            app.on_event(key(KeyCode::Esc));
        }
        assert!(app.chat_closed());
        // Another room than the one that was left.
        app.on_event(key(KeyCode::Down));
        let under_cursor = app.rooms[app.cursor_room().unwrap()].id.clone();
        app.perform(Action::RoomInfo);
        assert!(info(&app).rows().contains(&InfoRow::Pinned));
        app.info_activate(InfoRow::Pinned, 1);
        let Some(Modal::Pinned(pinned)) = &app.modal else {
            panic!("the pinned messages did not open");
        };
        assert_eq!(pinned.room_id, under_cursor);
        assert!(app.chat_closed());
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
        find(&text, "Enter info");
        find(&text, "Enter: user info");
        // Enter on a member opens User Info; inside it, Direct chat asks first.
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.modal,
            Some(Modal::UserInfo(popup)) if popup.user_id == "@carol:example.org"
        ));
        assert!(sent(&mut commands).is_empty());
        // The first Enter on Direct chat only looks for a chat that exists.
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
        let Modal::UserInfo(popup) = app.modal.as_ref().unwrap() else {
            panic!("user info closed");
        };
        assert_eq!(popup.confirm, Some(Confirm::NewChat(user.clone())));
        assert!(popup.busy.is_none());
        find(
            &screen(&mut app, &mut terminal),
            "Enter again to start one, Esc to stay",
        );
        assert!(sent(&mut commands).is_empty());
        // Esc, or walking away, withdraws it: the next Enter asks afresh.
        app.on_event(key(KeyCode::Esc));
        let Modal::UserInfo(popup) = app.modal.as_ref().unwrap() else {
            panic!("user info closed on confirm cancel");
        };
        assert_eq!(popup.confirm, None);
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
        click_on(&mut app, &mut terminal, " chat ");
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
        assert_eq!(
            info(&app).rows(),
            [
                InfoRow::Favourite,
                InfoRow::LowPriority,
                InfoRow::Pinned,
                InfoRow::Leave
            ]
        );
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
        // Favourite, Low priority, skip Pinned messages, Notifications in both directions, then leaving.
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Right));
        app.on_event(key(KeyCode::Left));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Down));
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
        find(&screen(&mut app, &mut small), "› Low priority");
        app.on_event(key(KeyCode::Down));
        find(&screen(&mut app, &mut small), "› Pinned messages");
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

    #[test]
    fn low_priority_section_order_toggles_and_mutual_exclusion() {
        assert_eq!(
            RoomKind::SECTIONS,
            [
                RoomKind::Invite,
                RoomKind::Knock,
                RoomKind::Favourite,
                RoomKind::Person,
                RoomKind::Room,
                RoomKind::LowPriority,
            ]
        );
        assert_eq!(RoomKind::LowPriority.title(), "Low priority");

        let (mut app, mut terminal, mut commands) = connected();
        let mut dm = summary("!dm:example.org", "Dan", RoomCategory::Person);
        dm.direct = true;
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            dm,
            summary("!fav:example.org", "Fav", RoomCategory::Favourite),
            summary("!inv:example.org", "Invited", RoomCategory::Invite),
        ]));
        sent(&mut commands);

        // Sidebar places Low priority after Rooms when synced, and direct DM keeps `@` prefix.
        let mut low_dm = summary("!dm:example.org", "Dan", RoomCategory::LowPriority);
        low_dm.direct = true;
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            low_dm,
            summary("!fav:example.org", "Fav", RoomCategory::Favourite),
            summary("!inv:example.org", "Invited", RoomCategory::Invite),
        ]));
        assert_eq!(app.rooms[1].kind, RoomKind::LowPriority);
        assert!(app.rooms[1].direct);
        let text = screen(&mut app, &mut terminal);
        find(&text, "Low priority");
        let order: Vec<_> = app
            .visible_rooms()
            .into_iter()
            .map(|i| app.rooms[i].name.as_str())
            .collect();
        assert_eq!(order, ["Invited", "Fav", "Rats", "Dan"]);

        // Room info toggles LowPriority <-> Favourite cleanly (mutually exclusive).
        app.open_room(0); // Rats
        app.on_event(key(KeyCode::F(4)));
        sent(&mut commands);
        answer(
            &mut app,
            ManageEvent::Info {
                room_id: ROOM.into(),
                result: Ok(Box::new(details())),
            },
        );
        // Select Favourite and turn it on.
        app.on_event(key(KeyCode::Enter));
        assert!(info(&app).details.as_ref().unwrap().favourite);
        assert!(!info(&app).details.as_ref().unwrap().low_priority);
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetFavourite { room_id, on: true }] if room_id == ROOM
        ));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::Favourite(true),
                result: Ok(()),
            },
        );
        // Step down to Low priority and turn it on -> Favourite clears optimistically.
        app.on_event(key(KeyCode::Down));
        assert_eq!(info(&app).current(), Some(InfoRow::LowPriority));
        app.on_event(key(KeyCode::Enter));
        assert!(info(&app).details.as_ref().unwrap().low_priority);
        assert!(!info(&app).details.as_ref().unwrap().favourite);
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetLowPriority { room_id, on: true }] if room_id == ROOM
        ));
        answer(
            &mut app,
            ManageEvent::Done {
                room_id: ROOM.into(),
                change: Change::LowPriority(true),
                result: Ok(()),
            },
        );
        assert_eq!(app.toast.as_ref().unwrap().0, "Marked low priority");
        app.on_event(key(KeyCode::Esc));
        sent(&mut commands); // StopInfo from closing RoomInfo

        // Server summary sync updates RoomKind::LowPriority; Command palette toggles it off.
        let mut dm2 = summary("!dm:example.org", "Dan", RoomCategory::LowPriority);
        dm2.direct = true;
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::LowPriority),
            dm2,
            summary("!fav:example.org", "Fav", RoomCategory::Favourite),
            summary("!inv:example.org", "Invited", RoomCategory::Invite),
        ]));
        assert_eq!(app.rooms[0].kind, RoomKind::LowPriority);
        app.on_event(ctrl('p'));
        type_text(&mut app, "Low priority");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetLowPriority { room_id, on: false }] if room_id == ROOM
        ));

        // Context menu on Dan (LowPriority) shows "Remove from low priority"; on Invited refuses.
        app.set_focus(Pane::Sidebar);
        app.sidebar.cursor = 3; // Dan
        app.perform(Action::ContextMenu);
        assert!(
            screen(&mut app, &mut terminal).contains("Remove from low priority"),
            "{}",
            screen(&mut app, &mut terminal)
        );
        app.on_event(key(KeyCode::Esc));

        app.open_room(3); // Invited
        sent(&mut commands);
        app.manage_click(Click::LowPriority);
        assert!(sent(&mut commands).is_empty());
    }

    fn all_commands(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
        let mut out = Vec::new();
        while let Ok(command) = commands.try_recv() {
            out.push(command);
        }
        out
    }

    #[test]
    fn browse_public_rooms_debounce_pagination_errors_and_redaction() {
        use crate::matrix::{PreviewJoinRule, PreviewMembership, PublicRoom, RoomPreview};

        let (mut app, mut terminal, mut commands) = connected();
        all_commands(&mut commands);

        // Choosing option 4 opens Browse with own HS hint and searches immediately.
        app.on_event(ctrl('n'));
        app.on_event(key(KeyCode::Char('5')));
        match &app.modal {
            Some(Modal::New(popup)) => {
                assert_eq!(popup.stage, Stage::Browse);
                assert_eq!(popup.dir_default_server.as_deref(), Some("example.org"));
                assert!(popup.dir_searching);
            }
            other => panic!("expected Browse modal, got {other:?}"),
        }
        let cmds = all_commands(&mut commands);
        assert!(matches!(
            cmds.as_slice(),
            [MatrixCmd::SearchDirectory { query, server: None, generation: g }]
                if query.is_empty() && *g > 0
        ));
        let g0 = app.dir_search_gen;

        // Typing cancels the active worker search, arms 250 ms debounce, and ignores stale results.
        type_text(&mut app, "rust");
        let cmds = all_commands(&mut commands);
        assert!(matches!(
            cmds.as_slice(),
            [MatrixCmd::CancelDirectorySearch]
        ));
        assert!(app.dir_search_debounce.is_some());
        let g1 = app.dir_search_gen;
        assert!(g1 > g0);

        app.on_matrix(MatrixEvent::DirectoryResults(DirectoryPage {
            generation: g0,
            append: false,
            rooms: vec![PublicRoom {
                room_id: "!stale:example.org".into(),
                name: Some("Stale".into()),
                alias: None,
                topic: None,
                avatar: None,
                joined_members: 1,
                world_readable: false,
                knock: false,
                joined: false,
            }],
            has_more: false,
        }));
        if let Some(Modal::New(popup)) = &app.modal {
            assert!(popup.dir_results.is_empty());
        }

        // Elapsed debounce dispatches the new search query.
        app.dir_search_debounce = Instant::now().checked_sub(Duration::from_millis(300));
        app.tick_dir_search();
        assert!(matches!(
            all_commands(&mut commands).as_slice(),
            [MatrixCmd::SearchDirectory { query, server: None, generation }]
                if query == "rust" && *generation == g1
        ));

        let page1 = DirectoryPage {
            generation: g1,
            append: false,
            rooms: vec![
                PublicRoom {
                    room_id: ROOM.into(),
                    name: Some("Rats".into()),
                    alias: Some("#rats:example.org".into()),
                    topic: Some("Already here".into()),
                    avatar: None,
                    joined_members: 42,
                    world_readable: true,
                    knock: false,
                    joined: true,
                },
                PublicRoom {
                    room_id: "!secret:example.org".into(),
                    name: Some("Secret Club".into()),
                    alias: Some("#club:example.org".into()),
                    topic: Some("Ask politely to enter".into()),
                    avatar: None,
                    joined_members: 15,
                    world_readable: false,
                    knock: true,
                    joined: false,
                },
                PublicRoom {
                    room_id: "!a:example.org".into(),
                    name: Some("Alpha".into()),
                    alias: None,
                    topic: None,
                    avatar: None,
                    joined_members: 5,
                    world_readable: false,
                    knock: false,
                    joined: false,
                },
                PublicRoom {
                    room_id: "!b:example.org".into(),
                    name: Some("Beta".into()),
                    alias: None,
                    topic: None,
                    avatar: None,
                    joined_members: 4,
                    world_readable: false,
                    knock: false,
                    joined: false,
                },
            ],
            has_more: true,
        };
        app.on_matrix(MatrixEvent::DirectoryResults(page1.clone()));
        insta::assert_snapshot!("directory_browse", screen(&mut app, &mut terminal));

        // Stepping near the bottom triggers pagination cleanly once and deduplicates.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        assert!(matches!(
            all_commands(&mut commands).as_slice(),
            [MatrixCmd::DirectoryNextPage { generation }] if *generation == g1
        ));
        app.on_matrix(MatrixEvent::DirectoryResults(DirectoryPage {
            generation: g1,
            append: true,
            rooms: vec![
                // Duplicate room ID must not be inserted twice.
                PublicRoom {
                    room_id: "!b:example.org".into(),
                    name: Some("Beta".into()),
                    alias: None,
                    topic: None,
                    avatar: None,
                    joined_members: 4,
                    world_readable: false,
                    knock: false,
                    joined: false,
                },
                PublicRoom {
                    room_id: "!c:example.org".into(),
                    name: Some("Gamma".into()),
                    alias: None,
                    topic: Some("Third page entry".into()),
                    avatar: None,
                    joined_members: 3,
                    world_readable: false,
                    knock: false,
                    joined: false,
                },
            ],
            has_more: false,
        }));
        if let Some(Modal::New(popup)) = &app.modal {
            assert_eq!(popup.dir_results.len(), 5);
            assert!(!popup.dir_has_more);
        }

        // Switching to Server field + error + Enter retries; Enter on joined room opens it directly.
        app.on_event(key(KeyCode::Tab));
        type_text(&mut app, "other.org");
        app.dir_search_debounce = Instant::now().checked_sub(Duration::from_millis(300));
        app.tick_dir_search();
        let g2 = app.dir_search_gen;
        assert!(matches!(
            all_commands(&mut commands).as_slice(),
            [MatrixCmd::SearchDirectory { query, server: Some(s), generation }]
                if query == "rust" && s == "other.org" && *generation == g2
        ));
        let err_event = MatrixEvent::DirectoryError {
            generation: g2,
            message: "other.org does not share a public room directory.".into(),
        };
        app.on_matrix(err_event);
        let err_screen = screen(&mut app, &mut terminal);
        find(&err_screen, "does not share a public room directory");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            all_commands(&mut commands).as_slice(),
            [MatrixCmd::SearchDirectory { server: Some(s), .. }] if s == "other.org"
        ));

        // Deliver joined entry and hit Enter -> closes popup and switches to ROOM directly.
        let g3 = app.dir_search_gen;
        app.on_matrix(MatrixEvent::DirectoryResults(DirectoryPage {
            generation: g3,
            append: false,
            rooms: vec![page1.rooms[0].clone()],
            has_more: false,
        }));
        app.open_room(1); // start in CHAT so switching to ROOM proves navigation
        all_commands(&mut commands);
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.room().map(|r| r.id.as_str()), Some(ROOM));

        // Redacting Debug checks.
        let secret_preview = RoomPreview {
            room_id: "!topsecret:other.org".into(),
            canonical_alias: Some("#hidden-alias:other.org".into()),
            name: Some("Private Name".into()),
            topic: Some("Confidential Topic".into()),
            avatar: Some("mxc://other.org/abc".into()),
            joined_members: 9,
            join_rule: PreviewJoinRule::Knock,
            encrypted: None,
            world_readable: false,
            membership: PreviewMembership::None,
            via: vec!["other.org".into()],
            space: false,
        };
        for dbg in [
            format!("{page1:?}"),
            format!("{:?}", page1.rooms[0]),
            format!("{secret_preview:?}"),
            format!(
                "{:?}",
                MatrixCmd::SearchDirectory {
                    query: "secretquery".into(),
                    server: Some("secret.org".into()),
                    generation: 1
                }
            ),
            format!(
                "{:?}",
                MatrixEvent::DirectoryError {
                    generation: 1,
                    message: "secret error body".into(),
                }
            ),
            format!(
                "{:?}",
                ManageCmd::Knock {
                    request: 1,
                    target: "#club:example.org".into(),
                    via: vec!["example.org".into()],
                    reason: Some("secret reason".into()),
                }
            ),
            format!("{:?}", NewPopup::new(Stage::Browse)),
        ] {
            for leaked in [
                "Rats",
                "Already here",
                "topsecret",
                "hidden-alias",
                "Private Name",
                "Confidential Topic",
                "secretquery",
                "secret.org",
                "secret error",
                "#club:example.org",
                "secret reason",
            ] {
                assert!(!dbg.contains(leaked), "leaked {leaked:?} in {dbg}");
            }
        }
    }

    #[test]
    fn preview_and_knock_flow_and_pending_withdraw() {
        use crate::matrix::{PreviewJoinRule, PreviewMembership, PublicRoom, RoomPreview};

        let (mut app, mut terminal, mut commands) = connected();
        all_commands(&mut commands);

        // Browse -> Enter on unjoined knock room -> Preview -> Esc back to Browse -> Re-enter -> Knock.
        app.on_event(ctrl('n'));
        app.on_event(key(KeyCode::Char('5')));
        let g = app.dir_search_gen;
        all_commands(&mut commands);
        app.on_matrix(MatrixEvent::DirectoryResults(DirectoryPage {
            generation: g,
            append: false,
            rooms: vec![PublicRoom {
                room_id: "!club:other.org".into(),
                name: Some("Tea Club".into()),
                alias: Some("#club:other.org".into()),
                topic: Some("Warm tea discussions".into()),
                avatar: None,
                joined_members: 18,
                world_readable: false,
                knock: true,
                joined: false,
            }],
            has_more: false,
        }));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.modal,
            Some(Modal::New(p)) if p.stage == Stage::Preview && p.preview_loading && p.preview_from_browse
        ));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Preview { target, via, .. }]
                if target == "#club:other.org" && via == &["other.org".to_owned()]
        ));

        let req = app.manage.requests;
        answer(
            &mut app,
            ManageEvent::Preview {
                request: req,
                result: Ok(Box::new(RoomPreview {
                    room_id: "!club:other.org".into(),
                    canonical_alias: Some("#club:other.org".into()),
                    name: Some("Tea Club".into()),
                    topic: Some("Warm tea discussions".into()),
                    avatar: None,
                    joined_members: 18,
                    join_rule: PreviewJoinRule::Knock,
                    encrypted: None,
                    world_readable: false,
                    membership: PreviewMembership::None,
                    via: vec!["other.org".into()],
                    space: false,
                })),
            },
        );
        insta::assert_snapshot!("room_preview_knock", screen(&mut app, &mut terminal));

        // Esc goes back to Browse preserving directory results; Enter opens Preview again.
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(
            &app.modal,
            Some(Modal::New(p)) if p.stage == Stage::Browse && p.dir_results.len() == 1
        ));
        app.on_event(key(KeyCode::Enter));
        sent(&mut commands);
        let req2 = app.manage.requests;
        answer(
            &mut app,
            ManageEvent::Preview {
                request: req2,
                result: Ok(Box::new(RoomPreview {
                    room_id: "!club:other.org".into(),
                    canonical_alias: Some("#club:other.org".into()),
                    name: Some("Tea Club".into()),
                    topic: Some("Warm tea discussions".into()),
                    avatar: None,
                    joined_members: 18,
                    join_rule: PreviewJoinRule::Knock,
                    encrypted: None,
                    world_readable: false,
                    membership: PreviewMembership::None,
                    via: vec!["other.org".into()],
                    space: false,
                })),
            },
        );

        // Move to Reason, type reason, Enter advances to Ask to join, Enter sends Knock.
        app.on_event(key(KeyCode::BackTab));
        type_text(&mut app, "I love oolong");
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Knock { target, reason: Some(r), .. }]
                if target == "#club:other.org" && r == "I love oolong"
        ));

        let knock_req = app.manage.requests;
        answer(
            &mut app,
            ManageEvent::Knocked {
                request: knock_req,
                result: Ok("!club:other.org".into()),
            },
        );
        assert!(app.modal.is_none());

        // Pending knock appears in sidebar and blocks member-only actions cleanly.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
            summary("!club:other.org", "Tea Club", RoomCategory::Knock),
        ]));
        assert_eq!(app.room().map(|r| r.id.as_str()), Some("!club:other.org"));
        assert_eq!(app.room().map(|r| r.kind), Some(RoomKind::Knock));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Pending knocks");
        find(&text, "asked to join this room");
        find(&text, "Withdraw");

        // Trying to invite / toggle favourite / open search on pending knock is refused.
        app.open_new(Stage::Invite);
        assert!(app.modal.is_none());
        assert_eq!(
            app.toast.as_ref().map(|t| t.0.as_str()),
            Some("Wait for your join request to be accepted first")
        );
        app.manage_click(Click::Favourite);
        assert!(sent(&mut commands).is_empty());

        // Palette labels Leave as "Withdraw join request for Tea Club…" and Room info uses Withdraw confirm.
        app.on_event(ctrl('p'));
        let pal_text = screen(&mut app, &mut terminal);
        assert!(
            !pal_text.contains("Decline the invitation"),
            "unexpected Invite copy in palette:\n{pal_text}"
        );
        app.on_event(key(KeyCode::Esc));

        app.on_event(key(KeyCode::F(4)));
        sent(&mut commands);
        app.on_event(key(KeyCode::Enter)); // first Enter on Leave -> "Withdraw request" confirm
        assert!(screen(&mut app, &mut terminal).contains("Enter again to withdraw"));
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        sent(&mut commands);

        // Clicking timeline Withdraw pill sends MatrixCmd::Leave.
        all_commands(&mut commands);
        app.perform(Action::WithdrawKnock);
        assert!(matches!(
            all_commands(&mut commands).as_slice(),
            [MatrixCmd::Leave { room_id }] if room_id == "!club:other.org"
        ));

        // Accepting our knock server-side promotes the room and triggers timeline load.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(CHAT, "bob", RoomCategory::Person),
            summary("!club:other.org", "Tea Club", RoomCategory::Room),
        ]));
        assert_eq!(app.room().map(|r| r.kind), Some(RoomKind::Room));
        let cmds = all_commands(&mut commands);
        assert!(cmds.iter().any(
            |c| matches!(c, MatrixCmd::OpenRoom { room_id, .. } if room_id == "!club:other.org")
        ));
        assert!(cmds.iter().any(
            |c| matches!(c, MatrixCmd::FetchMembers { room_id } if room_id == "!club:other.org")
        ));
    }

    #[test]
    fn link_opens_preview_and_membership_variants() {
        use crate::manage::PreviewButton;
        use crate::matrix::{PreviewJoinRule, PreviewMembership, RoomPreview};

        let (mut app, _terminal, mut commands) = connected();
        all_commands(&mut commands);
        assert!(app.try_open_matrix_link("https://matrix.to/#/#club:other.org"));
        assert!(matches!(
            &app.modal,
            Some(Modal::New(p))
                if p.stage == Stage::Preview
                    && p.preview_loading
                    && !p.preview_from_browse
                    && p.preview_target == "#club:other.org"
        ));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::Preview { target, via, .. }]
                if target == "#club:other.org" && via.is_empty()
        ));

        let mut base_preview = RoomPreview {
            room_id: "!club:other.org".into(),
            canonical_alias: Some("#club:other.org".into()),
            name: Some("Tea Club".into()),
            topic: None,
            avatar: None,
            joined_members: 4,
            join_rule: PreviewJoinRule::Knock,
            encrypted: Some(true),
            world_readable: false,
            membership: PreviewMembership::None,
            via: vec![],
            space: false,
        };

        let cases = [
            (
                PreviewMembership::Joined,
                vec![PreviewButton::Open, PreviewButton::Cancel],
            ),
            (
                PreviewMembership::Invited,
                vec![PreviewButton::Accept, PreviewButton::Cancel],
            ),
            (
                PreviewMembership::Knocked,
                vec![PreviewButton::Withdraw, PreviewButton::Cancel],
            ),
            (PreviewMembership::Banned, vec![PreviewButton::Cancel]),
        ];
        for (membership, expected) in cases {
            assert!(app.try_open_matrix_link("matrix:r/club:other.org"));
            sent(&mut commands);
            let req = app.manage.requests;
            let mut p = base_preview.clone();
            p.membership = membership;
            answer(
                &mut app,
                ManageEvent::Preview {
                    request: req,
                    result: Ok(Box::new(p)),
                },
            );
            let Some(Modal::New(popup)) = &app.modal else {
                panic!("expected Preview modal for {membership:?}");
            };
            assert_eq!(popup.preview_buttons(), expected);
        }

        // PreviewButton::Open on an already-known local room switches straight to it without re-joining.
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary("!club:other.org", "Tea Club", RoomCategory::Room),
        ]));
        app.open_room(0);
        all_commands(&mut commands);
        assert!(app.try_open_matrix_link("https://matrix.to/#/#club:other.org"));
        sent(&mut commands);
        let req = app.manage.requests;
        base_preview.membership = PreviewMembership::Joined;
        answer(
            &mut app,
            ManageEvent::Preview {
                request: req,
                result: Ok(Box::new(base_preview)),
            },
        );
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.room().map(|r| r.id.as_str()), Some("!club:other.org"));
        assert!(sent(&mut commands).is_empty());

        // Preview failure still allows falling back to Join / Ask to join / Cancel.
        assert!(app.try_open_matrix_link("https://matrix.to/#/%21broken:other.org"));
        sent(&mut commands);
        let req = app.manage.requests;
        answer(
            &mut app,
            ManageEvent::Preview {
                request: req,
                result: Err("unreachable".into()),
            },
        );
        let Some(Modal::New(popup)) = &app.modal else {
            panic!("expected fallback Preview modal");
        };
        assert_eq!(
            popup.preview_buttons(),
            vec![
                PreviewButton::Join,
                PreviewButton::Knock,
                PreviewButton::Cancel
            ]
        );
    }

    #[test]
    fn room_info_incoming_knock_requests_and_permissions() {
        let (mut app, mut terminal, mut commands) = connected();
        app.open_room(0);
        app.on_event(key(KeyCode::F(4)));
        all_commands(&mut commands);

        let carol = "@carol:example.org";
        let dave = "@dave:example.org";
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(RoomDetails {
                name: "Rats".into(),
                joined: 1,
                knock_requests: vec![
                    crate::matrix::RoomKnockRequest {
                        user_id: carol.into(),
                        name: "Carol".into(),
                        reason: Some("let me in".into()),
                        acceptable: true,
                        declinable: true,
                        bannable: true,
                    },
                    crate::matrix::RoomKnockRequest {
                        user_id: dave.into(),
                        name: "Dave".into(),
                        reason: None,
                        acceptable: true,
                        declinable: false,
                        bannable: false,
                    },
                ],
                members: vec![member("me", MemberRole::Admin, false)],
                ..RoomDetails::default()
            })),
        }));

        if let Some(Modal::RoomInfo(info)) = &mut app.modal {
            info.select(InfoRow::KnockRequest(0));
        }
        let text = screen(&mut app, &mut terminal);
        find(&text, "Join requests (2)");
        find(&text, "Carol");
        find(&text, "let me in");
        find(&text, "Dave");
        let accept_at = find(&text, " accept ");
        find(&text, " decline ");
        find(&text, " ban ");

        // Clicking `accept` pill sends AcceptKnock and waits.
        app.on_event(click(accept_at.0 + 1, accept_at.1));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::AcceptKnock { room_id, user_id }]
                if room_id == ROOM && user_id == carol
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::AcceptKnock,
            result: Ok(()),
        }));

        // Del asks to confirm declining on first press, then sends DeclineKnock on second.
        app.on_event(key(KeyCode::Delete));
        assert!(sent(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Del again to decline");
        app.on_event(key(KeyCode::Delete));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::DeclineKnock { room_id, user_id }]
                if room_id == ROOM && user_id == carol
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::DeclineKnock,
            result: Ok(()),
        }));

        // Alt-B opens BanPrompt and sends BanKnock with optional reason.
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        )));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Ban Carol");
        type_text(&mut app, "spam");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::BanKnock { room_id, user_id, reason: Some(r) }]
                if room_id == ROOM && user_id == carol && r == "spam"
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::BanKnock,
            result: Ok(()),
        }));

        // Dave only has `acceptable: true`: Del and Alt-B show permission errors cleanly.
        app.on_event(key(KeyCode::Down));
        assert_eq!(info(&app).current(), Some(InfoRow::KnockRequest(1)));
        app.on_event(key(KeyCode::Delete));
        assert!(sent(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "not allowed to decline");

        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::ALT,
        )));
        assert!(sent(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "not allowed to ban");

        // Enter on Dave accepts his request.
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::AcceptKnock { room_id, user_id }]
                if room_id == ROOM && user_id == dave
        ));
    }

    #[test]
    fn room_settings_toggle_all_views_and_guards() {
        use crate::matrix::{RoomPermissions, RoomSettingsPower};

        let (mut app, mut terminal, mut commands) = connected();
        app.open_room(0);
        app.on_event(key(KeyCode::F(4)));
        all_commands(&mut commands);

        let full_power = RoomSettingsPower {
            can_join_rule: true,
            can_history: true,
            can_encrypt: true,
            can_avatar: true,
            can_aliases: true,
            can_permissions: true,
        };
        let base_details = || RoomDetails {
            name: "Rats".into(),
            joined: 2,
            encrypted: Some(false),
            direct: false,
            join_rule: Some(RoomJoinRule::Invite),
            history_visibility: Some(RoomHistory::Invited),
            canonical_alias: Some("#main:example.org".into()),
            alt_aliases: vec!["#alt:example.org".into()],
            local_aliases: vec!["#main:example.org".into(), "#local:example.org".into()],
            own_homeserver: Some("example.org".into()),
            own_power_level: 100,
            settings_power: full_power,
            permissions: RoomPermissions {
                send_messages: 0,
                invite: 0,
                kick: 50,
                ban: 50,
                redact: 50,
                change_settings: 50,
                power_levels_event_level: 50,
                explicit_power_levels_rule: false,
            },
            members: vec![member("me", MemberRole::Admin, false)],
            ..RoomDetails::default()
        };

        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(base_details())),
        }));

        // Collapsed by default right above Members.
        let text = screen(&mut app, &mut terminal);
        find(&text, "▸ Room settings");
        assert!(!info(&app).settings_open);

        // Expand with Enter / Space / Right, collapse with Left.
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::SettingsToggle);
        }
        app.on_event(key(KeyCode::Right));
        assert!(info(&app).settings_open);
        let text = screen(&mut app, &mut terminal);
        find(&text, "▾ Room settings");
        find(&text, "Who can join");
        find(&text, "Permissions");
        insta::assert_snapshot!("room_info_settings_expanded", text);

        app.on_event(key(KeyCode::Left));
        assert!(!info(&app).settings_open);
        app.on_event(key(KeyCode::Char(' ')));
        assert!(info(&app).settings_open);

        // 1. Join rule picker.
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::JoinRule));
        }
        app.on_event(key(KeyCode::Enter));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Who can join");
        find(&text, "Invite only (current)");
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetJoinRule {
                room_id,
                rule: RoomJoinRule::Knock
            }] if room_id == ROOM
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::JoinRule,
            result: Ok(()),
        }));
        assert!(info(&app).setting_view.is_none());

        // Restricted join rule is clearly read-only and refuses editing.
        let mut restricted = base_details();
        restricted.join_rule = Some(RoomJoinRule::Restricted);
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(restricted)),
        }));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Restricted (read-only)");
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::JoinRule));
        }
        app.on_event(key(KeyCode::Enter));
        assert!(info(&app).setting_view.is_none());
        assert!(info(&app).error.is_some());

        // Restore editable details.
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(base_details())),
        }));

        // 2. History visibility shows prominent warning about future messages.
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::History));
        }
        app.on_event(key(KeyCode::Enter));
        let text = screen(&mut app, &mut terminal);
        find(&text, "WARNING:");
        find(&text, "only affects future");
        find(&text, "messages.");
        insta::assert_snapshot!("room_settings_history_warning", text);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetHistoryVisibility {
                room_id,
                visibility: RoomHistory::Joined
            }] if room_id == ROOM
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::HistoryVisibility,
            result: Ok(()),
        }));

        // 3. Encryption requires exact ENCRYPT confirmation and cannot be turned off once on.
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::Encryption));
        }
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "encrypt");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Type ENCRYPT in capitals");
        for _ in 0..7 {
            app.on_event(key(KeyCode::Backspace));
        }
        type_text(&mut app, "ENCRYPT");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::EnableEncryption { room_id }] if room_id == ROOM
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::EnableEncryption,
            result: Ok(()),
        }));
        let mut enc_on = base_details();
        enc_on.encrypted = Some(true);
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(enc_on)),
        }));
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::Encryption));
        }
        app.on_event(key(KeyCode::Enter));
        assert!(info(&app).setting_view.is_none());
        let text = screen(&mut app, &mut terminal);
        find(&text, "on · cannot be turned off");

        // 4. Room picture: shared Tab completion + dropped file paste + upload + remove.
        let pic_dir =
            std::env::temp_dir().join(format!("matrix-rat-room-pic-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pic_dir);
        std::fs::create_dir_all(&pic_dir).unwrap();
        let pic_file = pic_dir.join("room-crest.png");
        let mut png_bytes = Vec::new();
        crate::images::demo_picture(32, 20)
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        std::fs::write(&pic_file, &png_bytes).unwrap();

        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::Picture));
        }
        app.on_event(key(KeyCode::Enter));
        let prefix = pic_dir.join("room-cr").display().to_string();
        type_text(&mut app, &prefix);
        app.on_event(key(KeyCode::Tab));
        if let Some(SettingSubview::Picture { path, .. }) = &info(&app).setting_view {
            assert_eq!(path.as_str(), pic_file.display().to_string());
        } else {
            panic!("expected Picture subview");
        }
        app.on_event(key(KeyCode::Esc));

        // Drag-and-drop paste fills path and sets ready notice even if non-empty.
        app.on_event(key(KeyCode::Enter));
        let pasted = format!("'{}'\n", pic_file.display());
        app.on_event(Event::Paste(pasted));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Image file ready");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetRoomAvatar { room_id, path }]
                if room_id == ROOM && path == &pic_file
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::RoomAvatar,
            result: Ok(()),
        }));

        // Mid-edit permission withdrawal closes Picture subview.
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            info(&app).setting_view,
            Some(SettingSubview::Picture { .. })
        ));
        let mut no_pic = base_details();
        no_pic.settings_power.can_avatar = false;
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(no_pic)),
        }));
        assert!(info(&app).setting_view.is_none());
        assert!(info(&app).error.is_some());

        // Remove existing picture.
        let mut has_pic = base_details();
        has_pic.avatar_url = Some("mxc://example.org/pic".into());
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(has_pic)),
        }));
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::Picture));
        }
        app.on_event(key(KeyCode::Enter));
        let text = screen(&mut app, &mut terminal);
        let rem_btn = find(&text, "[Remove picture]");
        app.on_event(click(rem_btn.0 + 1, rem_btn.1));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::RemoveRoomAvatar { room_id }] if room_id == ROOM
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::RemoveRoomAvatar,
            result: Ok(()),
        }));
        let _ = std::fs::remove_dir_all(&pic_dir);

        // 5. Addresses subview: create local, publish alt, make main, unpublish, delete confirm.
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(base_details())),
        }));
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::Addresses));
        }
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "extra");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::CreateLocalAlias { room_id, alias }]
                if room_id == ROOM && alias == "#extra:example.org"
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::CreateLocalAlias,
            result: Ok(()),
        }));
        // Stays open on Addresses and clears input.
        assert!(matches!(
            info(&app).setting_view,
            Some(SettingSubview::Addresses { .. })
        ));

        // Publish input as alt.
        type_text(&mut app, "fresh-alt");
        let text = screen(&mut app, &mut terminal);
        let pub_btn = find(&text, "[Publish alt]");
        app.on_event(click(pub_btn.0 + 1, pub_btn.1));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetAddresses {
                room_id,
                canonical: Some(main),
                alt_aliases
            }] if room_id == ROOM
                && main == "#main:example.org"
                && *alt_aliases == ["#alt:example.org", "#fresh-alt:example.org"]
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::Addresses,
            result: Ok(()),
        }));

        // Select row 2 (`#local:example.org`) and promote via Make main; then Unpublish row 1.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        let text = screen(&mut app, &mut terminal);
        let mk = find(&text, "[Make main]");
        app.on_event(click(mk.0 + 1, mk.1));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetAddresses {
                room_id,
                canonical: Some(main),
                alt_aliases
            }] if room_id == ROOM
                && main == "#local:example.org"
                && *alt_aliases == ["#alt:example.org", "#main:example.org"]
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::Addresses,
            result: Ok(()),
        }));

        // Two-step delete confirmation for local alias, Esc cancels only confirm first.
        let text = screen(&mut app, &mut terminal);
        let del = find(&text, "[Delete local]");
        app.on_event(click(del.0 + 1, del.1));
        assert!(sent(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Delete local #local:example.org?");
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(
            info(&app).setting_view,
            Some(SettingSubview::Addresses {
                confirm_delete: None,
                ..
            })
        ));
        let text = screen(&mut app, &mut terminal);
        let del = find(&text, "[Delete local]");
        app.on_event(click(del.0 + 1, del.1));
        let text = screen(&mut app, &mut terminal);
        let del2 = find(&text, "[Delete local]");
        app.on_event(click(del2.0 + 1, del2.1));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::DeleteLocalAlias { room_id, alias }]
                if room_id == ROOM && alias == "#local:example.org"
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::DeleteLocalAlias,
            result: Ok(()),
        }));
        app.on_event(key(KeyCode::Esc));
        assert!(info(&app).setting_view.is_none());

        // 6. Direct chat toggle via Space.
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::Direct));
        }
        app.on_event(key(KeyCode::Char(' ')));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetDirect {
                room_id,
                direct: true
            }] if room_id == ROOM
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::Direct(true),
            result: Ok(()),
        }));

        // 7. Permissions subview: level cap guard for moderator (50) and save for admin (100).
        let mut mod_details = base_details();
        mod_details.own_power_level = 50;
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(mod_details)),
        }));
        if let Some(Modal::RoomInfo(i)) = &mut app.modal {
            i.select(InfoRow::Setting(RoomSetting::Permissions));
        }
        app.on_event(key(KeyCode::Enter));
        // Move to Invite -> Moderator (50), then Ban (row 3) -> Right tries 100 -> live error & blocked save.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Right));
        let text = screen(&mut app, &mut terminal);
        find(&text, "above your own level (50)");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));

        // Admin (100) changes Invite to Moderator (50), snapshots, and saves.
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(base_details())),
        }));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Right));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("room_settings_permissions", text);
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            sent(&mut commands).as_slice(),
            [ManageCmd::SetPermissions {
                room_id,
                permissions
            }] if room_id == ROOM && permissions.invite == 50
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::Permissions,
            result: Ok(()),
        }));
        assert!(info(&app).setting_view.is_none());

        // Read-only permissions view when can_permissions is false.
        let mut plain_member = base_details();
        plain_member.settings_power = RoomSettingsPower::default();
        plain_member.own_power_level = 0;
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(plain_member)),
        }));
        app.on_event(key(KeyCode::Enter));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Read-only: you do not have permission");
        app.on_event(key(KeyCode::Enter));
        assert!(info(&app).setting_view.is_none());
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn wheel_in_browse_respects_busy() {
        use crate::matrix::PublicRoom;

        let (mut app, _terminal, mut commands) = connected();
        let _ = all_commands(&mut commands);
        app.open_new_with(Stage::Browse, |_| {});
        let g = match all_commands(&mut commands).as_slice() {
            [MatrixCmd::SearchDirectory { generation, .. }] => *generation,
            other => panic!("expected SearchDirectory, got {other:?}"),
        };
        let rooms: Vec<PublicRoom> = (0..6)
            .map(|i| PublicRoom {
                room_id: format!("!r{i}:example.org"),
                name: Some(format!("Room {i}")),
                alias: None,
                topic: None,
                avatar: None,
                joined_members: 1,
                world_readable: false,
                knock: false,
                joined: false,
            })
            .collect();
        app.on_matrix(MatrixEvent::DirectoryResults(DirectoryPage {
            generation: g,
            append: false,
            rooms,
            has_more: true,
        }));
        if let Some(Modal::New(popup)) = &mut app.modal {
            popup.dir_page = 3;
            popup.busy = Some("Joining…");
        }
        app.scroll_dir_browse(3);
        if let Some(Modal::New(popup)) = &app.modal {
            assert_eq!(popup.dir_scroll, 0, "busy browse popup must not scroll");
            assert!(!popup.dir_loading_more);
        }
        assert!(
            all_commands(&mut commands).is_empty(),
            "busy browse popup must not request DirectoryNextPage"
        );

        // Once no longer busy, wheel scrolling near the end scrolls and loads the next page.
        if let Some(Modal::New(popup)) = &mut app.modal {
            popup.busy = None;
        }
        app.scroll_dir_browse(3);
        assert!(matches!(
            all_commands(&mut commands).as_slice(),
            [MatrixCmd::DirectoryNextPage { generation }] if *generation == g
        ));
    }
}
