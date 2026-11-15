//! UI handlers and worker integration for User Info and Own Profile popups.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use super::rooms::NO_ACCOUNT;
use crate::action::Pane;
use crate::complete::MentionMark;
use crate::lineedit::EditKeys;
use crate::manage::{Confirm, InfoRow};
use crate::matrix::{
    Change, ManageCmd, MatrixCmd, OwnProfile, OwnProfileAction, RoomMember, UserProfile, VerifyCmd,
    fallback_localpart,
};
use crate::modal::Modal;
use crate::model::MsgKind;
use crate::people::{
    OwnProfilePopup, UserAction, UserInfoCtx, UserInfoPopup, UserInfoRow, UserPending,
};
use crate::text::{is_blank, sanitize_line};

/// Maximum length of a Matrix display name.
const PROFILE_NAME_MAX: usize = 255;

impl App {
    fn next_user_info_request(&mut self) -> u64 {
        self.user_info_requests = self.user_info_requests.wrapping_add(1).max(1);
        self.user_info_requests
    }

    pub(crate) fn user_info_ctx(&self, user_id: &str) -> UserInfoCtx {
        UserInfoCtx {
            connected: self.matrix.is_some(),
            can_mention: self.room().is_some_and(|r| !r.is_pending()),
            is_self: self.user_id.as_deref() == Some(user_id),
            ignored: self.ignored_users.contains(user_id),
        }
    }

    /// Opens the User Info modal for `user_id`, fetching live profile details when connected.
    pub(super) fn open_user_info(
        &mut self,
        user_id: String,
        room_id: Option<String>,
        initial_name: Option<String>,
    ) {
        self.open_user_info_with(user_id, room_id, initial_name, None, false, 0, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn open_user_info_with(
        &mut self,
        user_id: String,
        room_id: Option<String>,
        initial_name: Option<String>,
        initial_member: Option<RoomMember>,
        own_is_creator: bool,
        own_power_level: i64,
        admin_count: usize,
    ) {
        let user_id = sanitize_line(user_id.trim());
        if user_id.is_empty() {
            return;
        }
        self.cancel_user_search();
        let initial_name = initial_name
            .map(|n| sanitize_line(&n))
            .filter(|n| !is_blank(n));
        let request = self.next_user_info_request();
        let connected = self.matrix.is_some();
        let popup = UserInfoPopup::new(
            user_id.clone(),
            room_id.clone(),
            initial_name,
            initial_member,
            own_is_creator,
            own_power_level,
            admin_count,
            request,
            connected,
        );
        self.open_modal(Modal::UserInfo(Box::new(popup)));
        if connected {
            self.send_matrix(MatrixCmd::FetchUserProfile {
                request,
                user_id,
                room_id,
            });
        }
    }

    /// Opens User Info for the sender of message `index` in the active timeline or thread.
    pub(super) fn open_user_info_from_sender(&mut self, index: usize) {
        let Some(msg) = self.messages().get(index) else {
            return;
        };
        let Some(user_id) = msg.sender_id.clone() else {
            self.show_toast("No user identity is available for this message");
            return;
        };
        let initial_name = (!is_blank(&msg.sender)).then(|| msg.sender.clone());
        let room_id = self.room_id();
        self.open_user_info(user_id, room_id, initial_name);
    }

    /// Opens User Info for the currently selected message's sender.
    pub(super) fn open_user_info_from_selection(&mut self) {
        let Some(index) = self.timeline.selected else {
            self.show_toast("Select a message first");
            return;
        };
        self.open_user_info_from_sender(index);
    }

    /// Opens User Info for a member row inside the Room Info popup.
    pub(super) fn open_user_info_from_room_member(&mut self, row: InfoRow) {
        let Some(Modal::RoomInfo(info)) = &self.modal else {
            return;
        };
        let Some(member) = info.member(row).cloned() else {
            return;
        };
        let room_id = info.room_id.clone();
        let own_is_creator = info.details.as_ref().is_some_and(|d| d.own_is_creator);
        let own_power_level = info.details.as_ref().map_or(0, |d| d.own_power_level);
        let admin_count = info.details.as_ref().map_or(0, |d| d.admin_count);
        let initial_name = (!is_blank(&member.name)).then(|| member.name.clone());
        self.open_user_info_with(
            member.user_id.clone(),
            Some(room_id),
            initial_name,
            Some(member),
            own_is_creator,
            own_power_level,
            admin_count,
        );
    }

    /// Opens User Info for a user-search match in the New/Direct/Invite popup.
    pub(super) fn open_user_info_from_search(&mut self, index: usize) {
        let (picked, invite_room) = {
            let Some(Modal::New(popup)) = &self.modal else {
                return;
            };
            if popup.busy.is_some() {
                return;
            }
            let Some(picked) = popup.user_results.get(index).cloned() else {
                return;
            };
            let room = popup.room.as_ref().map(|(id, _)| id.clone());
            (picked, room)
        };
        let room_id = invite_room.or_else(|| self.room_id());
        self.open_user_info(picked.user_id, room_id, picked.display_name);
    }

    /// Applies fetched profile data to the matching open User Info modal.
    pub(super) fn on_user_profile(&mut self, request: u64, result: Result<UserProfile, String>) {
        let ctx_uid = match &self.modal {
            Some(Modal::UserInfo(popup)) if popup.request == request => popup.user_id.clone(),
            _ => return,
        };
        let ctx = self.user_info_ctx(&ctx_uid);
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        popup.loading = false;
        match result {
            Ok(profile) => {
                if profile.own_is_creator
                    || profile.own_power_level != 0
                    || profile.admin_count != 0
                {
                    popup.own_is_creator = profile.own_is_creator;
                    popup.own_power_level = profile.own_power_level;
                    popup.admin_count = profile.admin_count;
                }
                if popup.error.is_none()
                    && popup.notice.is_none()
                    && let Some(note) = &profile.note
                {
                    popup.notice = Some(note.clone());
                }
                let prev_row = popup.current(&ctx);
                popup.profile = Some(profile);
                let rows = popup.rows(&ctx);
                if let Some(prev) = prev_row
                    && let Some(idx) = rows.iter().position(|r| *r == prev)
                {
                    popup.selected = idx;
                } else {
                    popup.selected = popup.selected.min(rows.len().saturating_sub(1));
                }
            }
            Err(err) => {
                popup.error = Some(err);
            }
        }
    }

    pub(super) fn on_user_info_ignore_done(
        &mut self,
        user_id: &str,
        ignored: bool,
        result: &Result<(), String>,
    ) {
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        if popup.user_id != user_id || !popup.answered(UserPending::Ignore(ignored)) {
            return;
        }
        match result {
            Ok(()) => {
                popup.error = None;
                popup.notice = Some(if ignored {
                    "User ignored".to_owned()
                } else {
                    "Removed from ignored list".to_owned()
                });
            }
            Err(err) => popup.error = Some(err.clone()),
        }
    }

    pub(super) fn on_user_info_manage_opened(
        &mut self,
        request: u64,
        result: &Result<String, String>,
    ) -> bool {
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return false;
        };
        if !popup.answered(UserPending::Chat(request)) {
            return false;
        }
        match result {
            Ok(room_id) => {
                self.modal = None;
                self.manage.record_pending_open(room_id.clone());
                self.open_pending_room();
            }
            Err(err) => popup.error = Some(err.clone()),
        }
        true
    }

    pub(super) fn on_user_info_no_direct_chat(&mut self, request: u64, user_id: &str) -> bool {
        let ctx = self.user_info_ctx(user_id);
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return false;
        };
        if !popup.answered(UserPending::Chat(request)) {
            return false;
        }
        if let Some(idx) = popup
            .rows(&ctx)
            .iter()
            .position(|r| *r == UserInfoRow::Action(UserAction::DirectChat))
        {
            popup.select(idx, &ctx);
        }
        popup.confirm = Some(Confirm::NewChat(user_id.to_owned()));
        true
    }

    pub(super) fn on_user_info_manage_done(
        &mut self,
        room_id: &str,
        change: Change,
        done_msg: &str,
        result: &Result<(), String>,
    ) -> bool {
        let refresh_args = {
            let Some(Modal::UserInfo(popup)) = &mut self.modal else {
                return false;
            };
            if popup.room_id.as_deref() != Some(room_id)
                || !popup.answered(UserPending::Manage(change))
            {
                return false;
            }
            match result {
                Ok(()) => {
                    popup.error = None;
                    if !done_msg.is_empty() {
                        popup.notice = Some(done_msg.to_owned());
                    }
                    Some((popup.user_id.clone(), popup.room_id.clone()))
                }
                Err(err) => {
                    popup.error = Some(err.clone());
                    None
                }
            }
        };
        if let Some((user_id, ctx_room)) = refresh_args
            && self.matrix.is_some()
        {
            let req = self.next_user_info_request();
            if let Some(Modal::UserInfo(popup)) = &mut self.modal {
                popup.request = req;
            }
            self.send_matrix(MatrixCmd::FetchUserProfile {
                request: req,
                user_id,
                room_id: ctx_room,
            });
        }
        true
    }

    pub(super) fn escape_user_info(&mut self) {
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        if popup.ban_prompt.take().is_some()
            || popup.role_chooser.take().is_some()
            || popup.confirm.take().is_some()
        {
            return;
        }
        self.modal = None;
    }

    pub(super) fn on_user_info_paste(&mut self, first_line: &str) {
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        if popup.busy.is_some() {
            return;
        }
        if let Some(prompt) = &mut popup.ban_prompt {
            prompt.reason.paste(first_line);
        }
    }

    pub(super) fn scroll_user_info(&mut self, lines: isize) {
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        popup.scroll = popup.scroll.saturating_add_signed(lines);
        popup.reveal = false;
    }

    pub(super) fn on_user_info_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if code == KeyCode::Esc {
            return self.escape_user_info();
        }
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let ctx_uid = match &self.modal {
            Some(Modal::UserInfo(popup)) => {
                if popup.busy.is_some() {
                    return;
                }
                popup.user_id.clone()
            }
            _ => return,
        };
        let ctx = self.user_info_ctx(&ctx_uid);
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        if let Some(prompt) = &mut popup.ban_prompt {
            if code == KeyCode::Enter && plain {
                self.submit_user_info_ban();
            } else {
                prompt.reason.handle_key(code, mods, EditKeys::FULL);
            }
            return;
        }
        if let Some(chooser) = &mut popup.role_chooser {
            match code {
                KeyCode::Up => chooser.step(-1),
                KeyCode::Char('k') if plain => chooser.step(-1),
                KeyCode::Down => chooser.step(1),
                KeyCode::Char('j') if plain => chooser.step(1),
                KeyCode::Enter if plain => self.submit_user_info_role(),
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Up | KeyCode::BackTab => popup.step(-1, &ctx),
            KeyCode::Char('k') if plain => popup.step(-1, &ctx),
            KeyCode::Down | KeyCode::Tab => popup.step(1, &ctx),
            KeyCode::Char('j') if plain => popup.step(1, &ctx),
            KeyCode::PageUp => popup.step_page(-1, &ctx),
            KeyCode::PageDown => popup.step_page(1, &ctx),
            KeyCode::Home => {
                popup.select(0, &ctx);
                popup.scroll = 0;
                popup.reveal = false;
            }
            KeyCode::End => {
                let last = popup.rows(&ctx).len().saturating_sub(1);
                popup.select(last, &ctx);
            }
            KeyCode::Char('c') if plain => self.copy_user_info_id(),
            KeyCode::Enter if plain => {
                if let Some(row) = popup.current(&ctx) {
                    self.activate_user_info_row(row);
                }
            }
            _ => {}
        }
    }

    pub(super) fn copy_user_info_id(&mut self) {
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        if popup.busy.is_some() {
            return;
        }
        let user_id = popup.user_id.clone();
        popup.error = None;
        popup.notice = Some("Copied Matrix ID".to_owned());
        self.pending_copy = Some(user_id);
        self.show_toast("Matrix ID copied");
    }

    pub(super) fn click_user_info_row(&mut self, index: usize) {
        let ctx_uid = match &self.modal {
            Some(Modal::UserInfo(popup)) if popup.busy.is_none() => popup.user_id.clone(),
            _ => return,
        };
        let ctx = self.user_info_ctx(&ctx_uid);
        let row = {
            let Some(Modal::UserInfo(popup)) = &mut self.modal else {
                return;
            };
            let rows = popup.rows(&ctx);
            let Some(row) = rows.get(index).copied() else {
                return;
            };
            popup.select(index, &ctx);
            row
        };
        self.activate_user_info_row(row);
    }

    pub(super) fn pick_user_info_role_option(&mut self, index: usize) {
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        let Some(chooser) = &mut popup.role_chooser else {
            return;
        };
        if index >= chooser.options.len() {
            return;
        }
        if chooser.selected != index {
            chooser.select(index);
        }
        self.submit_user_info_role();
    }

    pub(super) fn submit_user_info_ban(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        let Some(prompt) = popup.ban_prompt.take() else {
            return;
        };
        if !connected {
            popup.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        let Some(room_id) = popup.room_id.clone() else {
            popup.error = Some("No room context is active.".to_owned());
            return;
        };
        let reason = {
            let trimmed = sanitize_line(prompt.reason.as_str()).trim().to_owned();
            (!trimmed.is_empty()).then_some(trimmed)
        };
        popup.error = None;
        popup.notice = None;
        popup.wait_for(UserPending::Manage(Change::Ban), "Banning…");
        self.manage(ManageCmd::Ban {
            room_id,
            user_id: prompt.user_id,
            reason,
        });
    }

    fn submit_user_info_role(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::UserInfo(popup)) = &mut self.modal else {
            return;
        };
        let Some(chooser) = popup.role_chooser.as_mut() else {
            return;
        };
        let Some((level, _)) = chooser.chosen() else {
            return;
        };
        if level == chooser.current_level {
            popup.role_chooser = None;
            return;
        }
        if chooser.needs_last_admin_confirm(level) && !chooser.confirm_last_admin {
            chooser.confirm_last_admin = true;
            return;
        }
        let user_id = chooser.user_id.clone();
        popup.role_chooser = None;
        if !connected {
            popup.error = Some(NO_ACCOUNT.to_owned());
            return;
        }
        let Some(room_id) = popup.room_id.clone() else {
            popup.error = Some("No room context is active.".to_owned());
            return;
        };
        popup.error = None;
        popup.notice = None;
        popup.wait_for(UserPending::Manage(Change::SetRole), "Updating role…");
        self.manage(ManageCmd::SetRole {
            room_id,
            user_id,
            level,
        });
    }

    fn activate_user_info_row(&mut self, row: UserInfoRow) {
        let request = self.next_request();
        let ctx_uid = match &self.modal {
            Some(Modal::UserInfo(popup)) if popup.busy.is_none() => popup.user_id.clone(),
            _ => return,
        };
        let ctx = self.user_info_ctx(&ctx_uid);
        match row {
            UserInfoRow::SharedRoom(index) => {
                let target_id = {
                    let Some(Modal::UserInfo(popup)) = &self.modal else {
                        return;
                    };
                    popup.shared_rooms().get(index).map(|r| r.room_id.clone())
                };
                let Some(room_id) = target_id else {
                    return;
                };
                let Some(room_idx) = self.rooms.iter().position(|r| r.id == room_id) else {
                    if let Some(Modal::UserInfo(popup)) = &mut self.modal {
                        popup.error = Some("That room is not in your room list yet.".to_owned());
                    }
                    return;
                };
                self.modal = None;
                if self.space.is_some() && !self.space_rooms.contains(&room_id) {
                    self.select_space(None);
                }
                self.open_room(room_idx);
                if let Some(at) = self.visible_rooms().iter().position(|i| *i == room_idx) {
                    self.sidebar.cursor = at;
                    self.sidebar.reveal_cursor = true;
                }
            }
            UserInfoRow::Action(action) => {
                let disabled_reason = {
                    let Some(Modal::UserInfo(popup)) = &self.modal else {
                        return;
                    };
                    popup.action_disabled_reason(action, &ctx)
                };
                if let Some(reason) = disabled_reason {
                    if let Some(Modal::UserInfo(popup)) = &mut self.modal {
                        popup.clear_subprompts();
                        popup.notice = None;
                        popup.error = Some(reason.to_owned());
                    }
                    return;
                }

                match action {
                    UserAction::DirectChat => {
                        let (cmd, create) = {
                            let Some(Modal::UserInfo(popup)) = &mut self.modal else {
                                return;
                            };
                            popup.error = None;
                            popup.notice = None;
                            let confirmed = std::mem::take(&mut popup.confirm);
                            let create = confirmed == Some(Confirm::NewChat(popup.user_id.clone()));
                            let user_id = popup.user_id.clone();
                            popup.wait_for(
                                UserPending::Chat(request),
                                if create {
                                    "Starting the chat…"
                                } else {
                                    "Opening the chat…"
                                },
                            );
                            (
                                ManageCmd::DirectChat {
                                    request,
                                    user_id,
                                    create,
                                },
                                create,
                            )
                        };
                        let _ = create;
                        self.manage(cmd);
                    }
                    UserAction::Mention => {
                        let (user_id, name) = {
                            let Some(Modal::UserInfo(popup)) = &self.modal else {
                                return;
                            };
                            (popup.user_id.clone(), popup.display_name())
                        };
                        self.modal = None;
                        self.mention_user_in_composer(&user_id, &name);
                    }
                    UserAction::Verify => {
                        if self.verify.active().is_some() {
                            if let Some(Modal::UserInfo(popup)) = &mut self.modal {
                                popup.clear_subprompts();
                                popup.error =
                                    Some("Finish the active verification first.".to_owned());
                            }
                            return;
                        }
                        let user_id = {
                            let Some(Modal::UserInfo(popup)) = &mut self.modal else {
                                return;
                            };
                            popup.clear_subprompts();
                            popup.error = None;
                            popup.notice = Some("Verification requested…".to_owned());
                            popup.user_id.clone()
                        };
                        self.send_matrix(MatrixCmd::Verification(VerifyCmd::RequestUser {
                            user_id,
                        }));
                    }
                    UserAction::IgnoreToggle => {
                        let ignore_target = !ctx.ignored;
                        let expected = if ignore_target {
                            Confirm::Ignore(ctx_uid.clone())
                        } else {
                            Confirm::Unignore(ctx_uid.clone())
                        };
                        let confirmed = {
                            let Some(Modal::UserInfo(popup)) = &mut self.modal else {
                                return;
                            };
                            popup.error = None;
                            popup.notice = None;
                            popup.ban_prompt = None;
                            popup.role_chooser = None;
                            if popup.confirm.as_ref() != Some(&expected) {
                                popup.confirm = Some(expected);
                                return;
                            }
                            popup.confirm = None;
                            popup.wait_for(
                                UserPending::Ignore(ignore_target),
                                if ignore_target {
                                    "Ignoring…"
                                } else {
                                    "Unignoring…"
                                },
                            );
                            popup.user_id.clone()
                        };
                        self.send_ignore_user(confirmed, ignore_target);
                    }
                    UserAction::Role => {
                        if let Some(Modal::UserInfo(popup)) = &mut self.modal {
                            popup.open_role_chooser();
                        }
                    }
                    UserAction::Kick => {
                        let cmd = {
                            let Some(Modal::UserInfo(popup)) = &mut self.modal else {
                                return;
                            };
                            popup.error = None;
                            popup.notice = None;
                            let Some(member) = popup.effective_member().cloned() else {
                                return;
                            };
                            if !member.removable {
                                popup.confirm = None;
                                popup.error =
                                    Some("You are not allowed to remove this member.".to_owned());
                                return;
                            }
                            let expected = Confirm::Remove(popup.user_id.clone());
                            if popup.confirm.as_ref() != Some(&expected) {
                                popup.confirm = Some(expected);
                                return;
                            }
                            popup.confirm = None;
                            let Some(room_id) = popup.room_id.clone() else {
                                popup.error = Some("No room context is active.".to_owned());
                                return;
                            };
                            let user_id = popup.user_id.clone();
                            popup.wait_for(UserPending::Manage(Change::Kick), "Removing…");
                            ManageCmd::Kick { room_id, user_id }
                        };
                        self.manage(cmd);
                    }
                    UserAction::Ban => {
                        if let Some(Modal::UserInfo(popup)) = &mut self.modal {
                            let can_ban = popup
                                .effective_member()
                                .is_some_and(|member| member.bannable);
                            if !can_ban {
                                popup.clear_subprompts();
                                popup.error =
                                    Some("You are not allowed to ban this member.".to_owned());
                                return;
                            }
                            popup.open_ban_prompt();
                        }
                    }
                    UserAction::Unban => {
                        let cmd = {
                            let Some(Modal::UserInfo(popup)) = &mut self.modal else {
                                return;
                            };
                            popup.error = None;
                            popup.notice = None;
                            let can_unban = popup
                                .effective_member()
                                .is_some_and(|member| member.unbannable);
                            if !can_unban {
                                popup.confirm = None;
                                popup.error =
                                    Some("You are not allowed to unban this user.".to_owned());
                                return;
                            }
                            let expected = Confirm::Unban(popup.user_id.clone());
                            if popup.confirm.as_ref() != Some(&expected) {
                                popup.confirm = Some(expected);
                                return;
                            }
                            popup.confirm = None;
                            let Some(room_id) = popup.room_id.clone() else {
                                popup.error = Some("No room context is active.".to_owned());
                                return;
                            };
                            let user_id = popup.user_id.clone();
                            popup.wait_for(UserPending::Manage(Change::Unban), "Unbanning…");
                            ManageCmd::Unban { room_id, user_id }
                        };
                        self.manage(cmd);
                    }
                }
            }
        }
    }

    pub(super) fn mention_user_in_composer(&mut self, user_id: &str, display_name: &str) {
        let mut label = sanitize_line(display_name);
        if is_blank(&label) {
            label = fallback_localpart(user_id);
        }
        let cursor = self.composer.cursor().min(self.composer.text().len());
        let before = &self.composer.text()[..cursor];
        let after = &self.composer.text()[cursor..];
        let lead = if before.is_empty()
            || before.ends_with(char::is_whitespace)
            || before.ends_with('(')
            || before.ends_with('[')
        {
            ""
        } else {
            " "
        };
        let tail = match after.chars().next() {
            None => " ",
            Some(ch)
                if ch.is_whitespace()
                    || matches!(ch, ',' | '.' | ':' | ';' | '!' | '?' | ')' | ']') =>
            {
                ""
            }
            _ => " ",
        };
        let mention_at = cursor + lead.len();
        let insert = format!("{lead}{label}{tail}");
        self.insert_composer_text(&insert);
        self.complete
            .marks
            .push(MentionMark::user(label, user_id.to_owned()).with_at(Some(mention_at)));
        self.set_focus(Pane::Composer);
        self.sync_completion();
    }

    // ----- own profile ---------------------------------------------------------------

    pub(super) fn own_profile_summary(&self) -> String {
        let name = self
            .own_profile
            .as_ref()
            .and_then(|p| p.display_name.clone())
            .or_else(|| self.user_id.as_deref().map(fallback_localpart));
        let has_avatar = self
            .own_profile
            .as_ref()
            .is_some_and(|p| p.avatar.is_some());
        match (name, has_avatar) {
            (Some(n), true) => format!("{n} · photo"),
            (Some(n), false) => n,
            (None, _) => "Edit…".to_owned(),
        }
    }

    pub(super) fn own_profile_hint(&self) -> Option<String> {
        let uid = self.user_id.as_deref()?;
        Some(
            self.own_profile
                .as_ref()
                .and_then(|p| p.display_or_localpart(None, Some(uid)))
                .unwrap_or_else(|| fallback_localpart(uid)),
        )
    }

    pub(super) fn open_own_profile(&mut self) {
        let form = OwnProfilePopup::new(self.user_id.clone(), self.own_profile.as_ref());
        self.open_modal(Modal::OwnProfile(form));
    }

    pub(super) fn on_own_profile(&mut self, profile: OwnProfile) {
        self.own_profile = Some(profile.clone());
        let uid = self.user_id.clone();

        if let Some(Modal::OwnProfile(form)) = &mut self.modal {
            form.sync_from(&profile);
        }
        if let Some(Modal::UserInfo(info)) = &mut self.modal
            && uid.as_deref() == Some(info.user_id.as_str())
            && let Some(p) = &mut info.profile
        {
            p.display_name = profile
                .effective_display_name(info.room_id.as_deref())
                .map(str::to_owned);
            p.avatar = profile.avatar.clone();
        }

        let mut any_changed = false;
        let open_active = self.active;
        let has_thread = self.threads.open.is_some();

        for (idx, room) in self.rooms.iter_mut().enumerate() {
            let Some(eff) = profile.display_or_localpart(Some(&room.id), uid.as_deref()) else {
                continue;
            };
            let mut room_changed = false;
            for msg in &mut room.messages {
                if msg.own
                    && !matches!(msg.kind, MsgKind::System | MsgKind::ReadMarker)
                    && msg.sender != eff
                {
                    msg.sender = eff.clone();
                    room_changed = true;
                }
            }
            if room_changed && idx == open_active {
                any_changed = true;
            }
        }

        if has_thread
            && let Some(active_id) = self.rooms.get(open_active).map(|r| r.id.as_str())
            && let Some(eff) = profile.display_or_localpart(Some(active_id), uid.as_deref())
            && let Some(view) = &mut self.threads.open
        {
            view.rename_own_sender(&eff);
            any_changed = true;
        }

        if any_changed {
            self.row_cache = None;
            self.timeline_rev += 1;
            self.repaint = true;
        }
        self.dirty = true;
    }

    pub(super) fn on_own_profile_result(
        &mut self,
        what: OwnProfileAction,
        result: Result<(), String>,
    ) {
        let done = match &what {
            OwnProfileAction::DisplayName => "Display name updated",
            OwnProfileAction::Avatar => "Avatar updated",
            OwnProfileAction::RemoveAvatar => "Avatar removed",
        };
        match &mut self.modal {
            Some(Modal::OwnProfile(form)) if form.pending.as_ref() == Some(&what) => {
                form.pending = None;
                form.busy = None;
                match result {
                    Ok(()) => {
                        form.error = None;
                        form.notice = Some(done.to_owned());
                        match what {
                            OwnProfileAction::DisplayName => {
                                form.dirty_name = false;
                                let cleaned =
                                    crate::manage::clean_input(form.display_name.as_str().trim());
                                form.current_name = (!is_blank(&cleaned)).then_some(cleaned);
                            }
                            OwnProfileAction::Avatar => {
                                form.avatar_path.clear();
                            }
                            OwnProfileAction::RemoveAvatar => {
                                form.current_avatar = None;
                                form.set_focus(form.focus);
                            }
                        }
                    }
                    Err(err) => {
                        form.error = Some(err);
                    }
                }
            }
            _ => match result {
                Ok(()) => self.show_toast(done),
                Err(err) => self.show_toast(err),
            },
        }
    }

    pub(super) fn escape_own_profile(&mut self) {
        if let Some(Modal::OwnProfile(form)) = &self.modal
            && form.busy.is_some()
        {
            return;
        }
        self.modal = None;
    }

    pub(super) fn on_own_profile_paste(&mut self, full_text: &str, first_line: &str) {
        let Some(Modal::OwnProfile(form)) = &mut self.modal else {
            return;
        };
        if form.busy.is_some() {
            return;
        }
        form.error = None;
        form.notice = None;
        match super::dropped_single_image_path(full_text) {
            Ok(Some(path)) => {
                form.avatar_path.set(&path);
                form.set_focus(OwnProfilePopup::SET_AVATAR);
                return;
            }
            Ok(None) => {}
            Err(err) => {
                form.error = Some(err.to_owned());
                return;
            }
        }
        match form.focus {
            OwnProfilePopup::NAME_INPUT => {
                if form.display_name.paste(first_line).changed() {
                    form.dirty_name = true;
                }
            }
            OwnProfilePopup::AVATAR_PATH => {
                form.avatar_path.paste(first_line);
            }
            _ => {}
        }
    }

    pub(super) fn click_own_profile_slot(&mut self, slot: usize) {
        let Some(Modal::OwnProfile(form)) = &mut self.modal else {
            return;
        };
        if form.busy.is_some() {
            return;
        }
        form.set_focus(slot);
        match slot {
            OwnProfilePopup::SAVE_NAME => self.submit_own_display_name(),
            OwnProfilePopup::SET_AVATAR => self.submit_own_avatar(),
            OwnProfilePopup::REMOVE_AVATAR => self.submit_remove_avatar(),
            _ => {}
        }
    }

    pub(super) fn on_own_profile_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if code == KeyCode::Esc {
            return self.escape_own_profile();
        }
        let focus = match &self.modal {
            Some(Modal::OwnProfile(form)) => {
                if form.busy.is_some() {
                    return;
                }
                form.focus
            }
            _ => return,
        };
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match (code, mods) {
            (KeyCode::Tab, KeyModifiers::NONE) => {
                if focus == OwnProfilePopup::AVATAR_PATH && self.complete_own_avatar_path() {
                    return;
                }
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    form.step(1);
                }
            }
            (KeyCode::BackTab, _) => {
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    form.step(-1);
                }
            }
            (KeyCode::Up, KeyModifiers::NONE)
                if focus != OwnProfilePopup::NAME_INPUT
                    && focus != OwnProfilePopup::AVATAR_PATH =>
            {
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    form.step(-1);
                }
            }
            (KeyCode::Down, KeyModifiers::NONE)
                if focus != OwnProfilePopup::NAME_INPUT
                    && focus != OwnProfilePopup::AVATAR_PATH =>
            {
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    form.step(1);
                }
            }
            (KeyCode::Left, KeyModifiers::NONE) if focus == OwnProfilePopup::REMOVE_AVATAR => {
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    form.set_focus(OwnProfilePopup::SET_AVATAR);
                }
            }
            (KeyCode::Right, KeyModifiers::NONE) if focus == OwnProfilePopup::SET_AVATAR => {
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    form.set_focus(OwnProfilePopup::REMOVE_AVATAR);
                }
            }
            (KeyCode::Enter, _) if plain => match focus {
                OwnProfilePopup::NAME_INPUT | OwnProfilePopup::SAVE_NAME => {
                    self.submit_own_display_name();
                }
                OwnProfilePopup::AVATAR_PATH | OwnProfilePopup::SET_AVATAR => {
                    self.submit_own_avatar();
                }
                OwnProfilePopup::REMOVE_AVATAR => self.submit_remove_avatar(),
                _ => {}
            },
            _ => {
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    match focus {
                        OwnProfilePopup::NAME_INPUT => {
                            if form
                                .display_name
                                .handle_key(code, mods, EditKeys::FULL)
                                .changed()
                            {
                                form.dirty_name = true;
                                form.error = None;
                                form.notice = None;
                            }
                        }
                        OwnProfilePopup::AVATAR_PATH
                            if form
                                .avatar_path
                                .handle_key(code, mods, EditKeys::FULL)
                                .changed() =>
                        {
                            form.error = None;
                            form.notice = None;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn complete_own_avatar_path(&mut self) -> bool {
        let Some(Modal::OwnProfile(form)) = &mut self.modal else {
            return false;
        };
        match super::complete_path_field(&mut form.avatar_path) {
            super::PathCompleteResult::Empty => false,
            super::PathCompleteResult::Handled {
                notice,
                changed,
                has_options,
            } => {
                form.error = None;
                form.notice = notice;
                changed || has_options
            }
            super::PathCompleteResult::NoMatch => {
                form.notice = None;
                form.error = Some("No file or folder matches.".to_owned());
                true
            }
        }
    }

    fn submit_own_display_name(&mut self) {
        let connected = self.matrix.is_some();
        let Some(Modal::OwnProfile(form)) = &mut self.modal else {
            return;
        };
        form.error = None;
        form.notice = None;
        let cleaned = crate::manage::clean_input(form.display_name.as_str().trim());
        if cleaned.len() > PROFILE_NAME_MAX {
            form.error = Some("That display name is too long.".to_owned());
            return;
        }
        let target = (!is_blank(&cleaned)).then_some(cleaned);
        if target == form.current_name {
            form.dirty_name = false;
            form.notice = Some("Display name is already up to date.".to_owned());
            return;
        }
        if !connected || self.user_id.is_none() {
            form.error = Some(if self.user_id.is_none() {
                "Sign in to update your display name.".to_owned()
            } else {
                NO_ACCOUNT.to_owned()
            });
            return;
        }
        form.busy = Some("Saving display name…");
        form.pending = Some(OwnProfileAction::DisplayName);
        self.send_matrix(MatrixCmd::SetOwnDisplayName { name: target });
    }

    fn submit_own_avatar(&mut self) {
        let connected = self.matrix.is_some();
        let limit = self.attach.limit;
        let signed_in = self.user_id.is_some();
        let raw = match &mut self.modal {
            Some(Modal::OwnProfile(form)) => {
                form.error = None;
                form.notice = None;
                form.avatar_path.as_str().trim().to_owned()
            }
            _ => return,
        };
        let path = match super::validate_avatar_path(&raw, limit) {
            Ok(path) => path,
            Err(err) => {
                if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                    form.error = Some(err);
                }
                return;
            }
        };
        if !connected || !signed_in {
            if let Some(Modal::OwnProfile(form)) = &mut self.modal {
                form.error = Some(if !signed_in {
                    "Sign in to update your avatar.".to_owned()
                } else {
                    NO_ACCOUNT.to_owned()
                });
            }
            return;
        }
        if let Some(Modal::OwnProfile(form)) = &mut self.modal {
            form.busy = Some("Uploading avatar…");
            form.pending = Some(OwnProfileAction::Avatar);
        }
        self.send_matrix(MatrixCmd::SetOwnAvatar { path });
    }

    fn submit_remove_avatar(&mut self) {
        let connected = self.matrix.is_some();
        let signed_in = self.user_id.is_some();
        let Some(Modal::OwnProfile(form)) = &mut self.modal else {
            return;
        };
        form.error = None;
        form.notice = None;
        if form.current_avatar.is_none() {
            return;
        }
        if !connected || !signed_in {
            form.error = Some(if !signed_in {
                "Sign in to remove your avatar.".to_owned()
            } else {
                NO_ACCOUNT.to_owned()
            });
            return;
        }
        form.busy = Some("Removing avatar…");
        form.pending = Some(OwnProfileAction::RemoveAvatar);
        self.send_matrix(MatrixCmd::RemoveOwnAvatar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;

    use crossterm::event::{Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::action::Action;
    use crate::complete::MentionKind;
    use crate::config::Config;
    use crate::manage::InfoRow;
    use crate::matrix::{
        ManageEvent, MatrixEvent, MatrixHandle, MemberRole, NotifyLevel, RoomCategory, RoomDetails,
        RoomSummary, UserMatch, UserSharedRoom, UserVerifyStatus, VerifyStage, VerifyUpdate,
    };
    use crate::modal::Setting;
    use crate::model::{ItemRef, Message};
    use crate::ui;

    const ROOM: &str = "!rats:example.org";
    const OTHER: &str = "!other:example.org";
    const DM: &str = "!dm:example.org";
    const BOB: &str = "@bob:example.org";
    const CAROL: &str = "@carol:example.org";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "matrix-rat-people-app-{}-{}-{n}",
                std::process::id(),
                name
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, rel: &str, content: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
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

    fn click_on(app: &mut App, terminal: &mut Terminal<TestBackend>, needle: &str) {
        let text = screen(app, terminal);
        let (col, row) = find(&text, needle);
        app.on_event(click(col + 1, row));
    }

    fn drain_cmds(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = commands.try_recv() {
            out.push(cmd);
        }
        out
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
            marked_unread: false,
            preview: None,
            avatar: None,
            alias: None,
            direct: category == RoomCategory::Person,
            muted: false,
        }
    }

    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = Config::default();
        let mut app = App::new(&config, PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@alice:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Rats", RoomCategory::Room),
            summary(OTHER, "Design", RoomCategory::Room),
            summary(DM, "Bob", RoomCategory::Person),
        ]));
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM.into(),
            generation: app.generation,
            messages: vec![
                Message {
                    id: "e1".into(),
                    item: Some(ItemRef::Event("$ev1:example.org".into())),
                    sender: "Bob".into(),
                    sender_id: Some(BOB.into()),
                    body: "hello https://matrix.to/#/@bob:example.org".into(),
                    day: "Today".into(),
                    time: "12:00".into(),
                    kind: MsgKind::Text,
                    own: false,
                    ..Message::default()
                },
                Message {
                    id: "e2".into(),
                    item: Some(ItemRef::Event("$ev2:example.org".into())),
                    sender: "alice".into(),
                    sender_id: Some("@alice:example.org".into()),
                    body: "hi back".into(),
                    day: "Today".into(),
                    time: "12:01".into(),
                    kind: MsgKind::Text,
                    own: true,
                    ..Message::default()
                },
            ],
            reached_start: false,
            detached: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal, commands)
    }

    fn sample_details() -> RoomDetails {
        RoomDetails {
            name: "Rats".into(),
            topic: "Topic".into(),
            alias: Some("#rats:example.org".into()),
            encrypted: Some(true),
            direct: false,
            joined: 3,
            invited: 0,
            favourite: false,
            low_priority: false,
            notify: Some(NotifyLevel::All),
            can_rename: false,
            can_set_topic: false,
            can_invite: true,
            own_display_name: None,
            members: vec![
                RoomMember {
                    user_id: "@alice:example.org".into(),
                    name: "alice".into(),
                    role: MemberRole::Admin,
                    power_level: 100,
                    invited: false,
                    own: true,
                    removable: false,
                    role_editable: true,
                    ..RoomMember::default()
                },
                RoomMember {
                    user_id: BOB.into(),
                    name: "Bob".into(),
                    role: MemberRole::User,
                    power_level: 0,
                    invited: false,
                    own: false,
                    removable: true,
                    bannable: true,
                    role_editable: true,
                    ..RoomMember::default()
                },
                RoomMember {
                    user_id: CAROL.into(),
                    name: "Carol".into(),
                    role: MemberRole::Moderator,
                    power_level: 50,
                    invited: false,
                    own: false,
                    removable: true,
                    bannable: true,
                    role_editable: true,
                    ..RoomMember::default()
                },
            ],
            members_error: None,
            members_loading: false,
            members_partial: false,
            ..RoomDetails::default()
        }
    }

    fn user_info(app: &App) -> &UserInfoPopup {
        match &app.modal {
            Some(Modal::UserInfo(popup)) => popup,
            other => panic!("expected Modal::UserInfo, got {other:?}"),
        }
    }

    #[test]
    fn user_info_opens_from_timeline_room_info_menu_palette_search_and_link() {
        let (mut app, mut terminal, mut commands) = connected();
        drain_cmds(&mut commands);

        // 1. Clicking sender header in timeline opens User Info and requests profile.
        click_on(&mut app, &mut terminal, "Bob  12:00");
        assert_eq!(user_info(&app).user_id, BOB);
        assert!(drain_cmds(&mut commands).iter().any(|c| matches!(
            c,
            MatrixCmd::FetchUserProfile { user_id, room_id, .. }
                if user_id == BOB && room_id.as_deref() == Some(ROOM)
        )));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        // 2. Message context menu -> "User info".
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        app.perform(Action::ContextMenu);
        click_on(&mut app, &mut terminal, "User info");
        assert_eq!(user_info(&app).user_id, BOB);
        app.on_event(key(KeyCode::Esc));
        drain_cmds(&mut commands);

        // 3. Command palette -> Action::UserInfo opens popup for selected message sender.
        app.perform(Action::OpenPalette);
        type_text(&mut app, "User info");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(user_info(&app).user_id, BOB);
        app.on_event(key(KeyCode::Esc));
        drain_cmds(&mut commands);

        // 4. Room Info -> Enter on member opens User Info cleanly.
        app.on_event(key(KeyCode::F(4)));
        drain_cmds(&mut commands);
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(sample_details())),
        }));
        type_text(&mut app, "carol");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(user_info(&app).user_id, CAROL);
        assert_eq!(user_info(&app).display_name(), "Carol");
        app.on_event(key(KeyCode::Esc));
        drain_cmds(&mut commands);

        // 5. User-search Stage::Invite " info " pill opens User Info.
        app.on_event(key(KeyCode::F(4)));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(sample_details())),
        }));
        click_on(&mut app, &mut terminal, "Invite someone");
        type_text(&mut app, "bob");
        app.on_matrix(MatrixEvent::UserSearchResults {
            generation: app.user_search_gen,
            results: vec![UserMatch {
                user_id: BOB.into(),
                display_name: Some("Bob".into()),
                avatar: None,
            }],
        });
        app.on_event(key(KeyCode::Down));
        click_on(&mut app, &mut terminal, " info ");
        assert_eq!(user_info(&app).user_id, BOB);
        app.on_event(key(KeyCode::Esc));

        // 6. matrix.to user link opens User Info even with no room context.
        assert!(app.try_open_matrix_link("https://matrix.to/#/@stranger:example.org"));
        assert_eq!(user_info(&app).user_id, "@stranger:example.org");
    }

    #[test]
    fn user_info_actions_dm_mention_verify_ignore_copy_and_shared_rooms() {
        let (mut app, mut terminal, mut commands) = connected();
        app.open_user_info(BOB.into(), Some(ROOM.into()), Some("Bob".into()));
        let req = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };

        // Stale response with wrong request id is dropped.
        app.on_matrix(MatrixEvent::UserProfile {
            request: req.wrapping_add(99),
            result: Ok(Box::new(UserProfile {
                user_id: BOB.into(),
                display_name: Some("Wrong Stale Name".into()),
                ..UserProfile::default()
            })),
        });
        assert!(user_info(&app).profile.is_none());

        // Matching response populates display name, shared rooms, and verify state.
        app.on_matrix(MatrixEvent::UserProfile {
            request: req,
            result: Ok(Box::new(UserProfile {
                user_id: BOB.into(),
                display_name: Some("Bob Ross".into()),
                verify: UserVerifyStatus::Unverified,
                shared_rooms: vec![
                    UserSharedRoom {
                        room_id: OTHER.into(),
                        name: "Design".into(),
                        direct: false,
                        invited: false,
                    },
                    UserSharedRoom {
                        room_id: DM.into(),
                        name: "Bob".into(),
                        direct: true,
                        invited: false,
                    },
                ],
                room_member: sample_details().members.into_iter().nth(1),
                room_name: Some("Rats".into()),
                own_power_level: 100,
                admin_count: 2,
                ..UserProfile::default()
            })),
        });
        assert_eq!(user_info(&app).display_name(), "Bob Ross");

        // Pressing `c` or clicking `[copy]` copies user ID cleanly.
        app.on_event(key(KeyCode::Char('c')));
        assert_eq!(app.pending_copy.take().as_deref(), Some(BOB));
        click_on(&mut app, &mut terminal, "[copy]");
        assert_eq!(app.pending_copy.take().as_deref(), Some(BOB));

        // Direct chat tries existing room first, then asks before creating on NoDirectChat.
        click_on(&mut app, &mut terminal, "Direct chat");
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::DirectChat { user_id, create: false, .. })]
                if user_id == BOB
        ));
        let open_req = match user_info(&app).pending {
            Some(UserPending::Chat(req)) => req,
            ref other => panic!("expected UserPending::Chat, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::Manage(ManageEvent::NoDirectChat {
            request: open_req,
            user_id: BOB.into(),
        }));
        find(&screen(&mut app, &mut terminal), "Enter again to start one");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::DirectChat { user_id, create: true, .. })]
                if user_id == BOB
        ));
        let create_req = match user_info(&app).pending {
            Some(UserPending::Chat(req)) => req,
            ref other => panic!("expected UserPending::Chat, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Opened {
            request: create_req,
            result: Ok(DM.into()),
        }));
        assert!(app.modal.is_none());
        assert_eq!(app.room().unwrap().id, DM);

        // Mention inserts MentionMark::user into composer and moves focus to composer.
        app.open_user_info(BOB.into(), Some(DM.into()), Some("Bob Ross".into()));
        let req2 = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::UserProfile {
            request: req2,
            result: Ok(Box::new(UserProfile {
                user_id: BOB.into(),
                display_name: Some("Bob Ross".into()),
                verify: UserVerifyStatus::NoKeys,
                shared_rooms: vec![UserSharedRoom {
                    room_id: OTHER.into(),
                    name: "Design".into(),
                    direct: false,
                    invited: false,
                }],
                ..UserProfile::default()
            })),
        });
        // Verify with NoKeys refuses cleanly without sending VerifyCmd.
        click_on(&mut app, &mut terminal, "Verify user");
        assert!(drain_cmds(&mut commands).is_empty());
        assert!(
            user_info(&app)
                .error
                .as_deref()
                .is_some_and(|e| e.contains("no encryption keys"))
        );

        // Ignore toggle asks first, then sends IgnoreUser and live-updates popup state.
        click_on(&mut app, &mut terminal, "Ignore user");
        find(&screen(&mut app, &mut terminal), "Enter again to ignore");
        assert!(drain_cmds(&mut commands).is_empty());
        click_on(&mut app, &mut terminal, "Ignore user");
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::IgnoreUser { user_id }] if user_id == BOB
        ));
        app.on_matrix(MatrixEvent::IgnoreResult {
            user_id: BOB.into(),
            ignored: true,
            result: Ok(()),
        });
        assert!(user_info(&app).busy.is_none());
        find(&screen(&mut app, &mut terminal), "Stop ignoring");

        // Shared room jump clears space filter when needed and switches to room.
        app.space = Some("!space:example.org".into());
        app.space_rooms.clear();
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "│   Design");
        app.on_event(click(col + 4, row));
        assert!(app.modal.is_none());
        assert!(app.space.is_none());
        assert_eq!(app.room().unwrap().id, OTHER);

        // Verify when Unverified sends VerifyCmd::RequestUser and allows verify modal handover.
        app.open_user_info(CAROL.into(), Some(OTHER.into()), Some("Carol".into()));
        let req3 = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::UserProfile {
            request: req3,
            result: Ok(Box::new(UserProfile {
                user_id: CAROL.into(),
                display_name: Some("Carol".into()),
                verify: UserVerifyStatus::Unverified,
                ..UserProfile::default()
            })),
        });
        click_on(&mut app, &mut terminal, "Verify user");
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Verification(VerifyCmd::RequestUser { user_id })] if user_id == CAROL
        ));
        app.on_matrix(MatrixEvent::Verification(VerifyUpdate {
            flow: 1,
            peer: CAROL.into(),
            own: false,
            this_device: false,
            stage: VerifyStage::Waiting,
        }));
        assert!(matches!(app.modal, Some(Modal::Verify(_))));
        app.on_event(key(KeyCode::Esc));

        // Opening a Verified user hides "Verify user" and preserves non-Verify selection.
        app.open_user_info(BOB.into(), Some(ROOM.into()), Some("Bob".into()));
        // Navigate down to Ignore user while loading (Direct chat=0, Mention=1, Verify=2, Ignore=3).
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        let ver_req = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::UserProfile {
            request: ver_req,
            result: Ok(Box::new(UserProfile {
                user_id: BOB.into(),
                display_name: Some("Bob".into()),
                verify: UserVerifyStatus::Verified,
                ..UserProfile::default()
            })),
        });
        assert!(!screen(&mut app, &mut terminal).contains("Verify user"));
        assert_eq!(
            user_info(&app).current(&app.user_info_ctx(BOB)),
            Some(UserInfoRow::Action(UserAction::IgnoreToggle))
        );
        app.on_event(key(KeyCode::Esc));

        // Mention inserts display name mark into composer and focuses composer.
        app.open_user_info(BOB.into(), Some(OTHER.into()), Some("Bob Ross".into()));
        click_on(&mut app, &mut terminal, "Mention in composer");
        assert!(app.modal.is_none());
        assert_eq!(app.focus, Pane::Composer);
        assert_eq!(app.composer.text(), "Bob Ross ");
        assert!(
            app.complete.marks.iter().any(|m| {
                matches!(&m.kind, MentionKind::User { user_id, .. } if user_id == BOB)
            })
        );
    }

    #[test]
    fn user_info_moderation_role_kick_ban_unban_and_snapshots() {
        let (mut app, mut terminal, mut commands) = connected();
        app.open_user_info(BOB.into(), Some(ROOM.into()), Some("Bob".into()));
        let req = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::UserProfile {
            request: req,
            result: Ok(Box::new(UserProfile {
                user_id: BOB.into(),
                display_name: Some("Bob".into()),
                verify: UserVerifyStatus::Verified,
                shared_rooms: vec![UserSharedRoom {
                    room_id: OTHER.into(),
                    name: "Design".into(),
                    direct: false,
                    invited: false,
                }],
                room_member: sample_details().members.into_iter().nth(1),
                room_name: Some("Rats".into()),
                own_power_level: 100,
                admin_count: 1,
                ..UserProfile::default()
            })),
        });

        insta::assert_snapshot!("user_info_modal", screen(&mut app, &mut terminal));

        // Change role -> RoleChooser -> choose Moderator -> SetRole command.
        click_on(&mut app, &mut terminal, "Change role");
        assert!(user_info(&app).role_chooser.is_some());
        click_on(&mut app, &mut terminal, "Moderator");
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::SetRole { room_id, user_id, level: 50 })]
                if room_id == ROOM && user_id == BOB
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::SetRole,
            result: Ok(()),
        }));
        assert_eq!(user_info(&app).notice.as_deref(), Some("Role updated"));

        // Self-demotion with admin_count == 1 asks for last-admin confirmation.
        app.open_user_info(
            "@alice:example.org".into(),
            Some(ROOM.into()),
            Some("alice".into()),
        );
        let self_req = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::UserProfile {
            request: self_req,
            result: Ok(Box::new(UserProfile {
                user_id: "@alice:example.org".into(),
                display_name: Some("alice".into()),
                verify: UserVerifyStatus::Verified,
                room_member: sample_details().members.into_iter().next(),
                room_name: Some("Rats".into()),
                own_power_level: 100,
                admin_count: 1,
                own: true,
                ..UserProfile::default()
            })),
        });
        click_on(&mut app, &mut terminal, "Change role");
        click_on(&mut app, &mut terminal, "Moderator");
        assert!(drain_cmds(&mut commands).is_empty());
        assert!(
            user_info(&app)
                .role_chooser
                .as_ref()
                .unwrap()
                .confirm_last_admin
        );
        click_on(&mut app, &mut terminal, "Moderator");
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::SetRole { user_id, level: 50, .. })]
                if user_id == "@alice:example.org"
        ));

        // Kick + inline Ban on target.
        app.open_user_info(BOB.into(), Some(ROOM.into()), Some("Bob".into()));
        let req2 = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::UserProfile {
            request: req2,
            result: Ok(Box::new(UserProfile {
                user_id: BOB.into(),
                display_name: Some("Bob".into()),
                room_member: sample_details().members.into_iter().nth(1),
                room_name: Some("Rats".into()),
                own_power_level: 100,
                admin_count: 2,
                ..UserProfile::default()
            })),
        });
        click_on(&mut app, &mut terminal, "Remove from room");
        find(&screen(&mut app, &mut terminal), "Enter again to remove");
        click_on(&mut app, &mut terminal, "Remove from room");
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::Kick { user_id, .. })] if user_id == BOB
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::Kick,
            result: Ok(()),
        }));
        drain_cmds(&mut commands);

        click_on(&mut app, &mut terminal, "Ban from room");
        assert!(user_info(&app).ban_prompt.is_some());
        app.on_paste("Spammer\nsecond line");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::Ban { user_id, reason: Some(reason), .. })]
                if user_id == BOB && reason == "Spammer"
        ));
    }

    #[test]
    fn own_profile_badge_completion_validation_avatar_and_snapshot() {
        let scratch = Scratch::new("avatar");
        let txt = scratch.file("notes.txt", b"hello");
        let mut png_bytes = Vec::new();
        crate::images::demo_picture(32, 20)
            .write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();
        let img = scratch.file("avatar_alice.png", &png_bytes);
        let _other_img = scratch.file("avatar_bob.png", &png_bytes);

        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::OwnProfile(OwnProfile {
            display_name: Some("Alice M".into()),
            avatar: Some("mxc://example.org/av1".into()),
            ..OwnProfile::default()
        }));
        drain_cmds(&mut commands);

        assert_eq!(app.own_profile_summary(), "Alice M · photo");

        // Clicking composer top-border badge opens Your profile modal.
        click_on(&mut app, &mut terminal, "Alice M ─");
        assert!(matches!(app.modal, Some(Modal::OwnProfile(_))));
        insta::assert_snapshot!("own_profile_modal", screen(&mut app, &mut terminal));

        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        // Settings -> Profile opens the same popup.
        app.perform_setting(Setting::Profile);
        assert!(matches!(app.modal, Some(Modal::OwnProfile(_))));

        // Display name validation (> 255 chars refused) + valid save.
        app.on_event(key(KeyCode::Backspace));
        for _ in 0..8 {
            app.on_event(key(KeyCode::Backspace));
        }
        let too_long = "a".repeat(PROFILE_NAME_MAX + 1);
        type_text(&mut app, &too_long);
        app.on_event(key(KeyCode::Enter));
        assert!(drain_cmds(&mut commands).is_empty());
        assert!(matches!(
            &app.modal,
            Some(Modal::OwnProfile(f)) if f.error.as_deref().is_some_and(|e| e.contains("too long"))
        ));

        // Clear and set valid display name.
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        type_text(&mut app, "Alice Updated");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::SetOwnDisplayName { name: Some(n) }] if n == "Alice Updated"
        ));
        app.on_matrix(MatrixEvent::OwnProfileResult {
            what: OwnProfileAction::DisplayName,
            result: Ok(()),
        });
        assert!(matches!(
            &app.modal,
            Some(Modal::OwnProfile(f)) if f.notice.as_deref() == Some("Display name updated")
        ));

        // Tab to avatar path field: Tab completes common prefix + shows options, then completes exact match.
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Tab));
        let prefix = format!("{}/av", scratch.0.to_string_lossy());
        type_text(&mut app, &prefix);
        app.on_event(key(KeyCode::Tab));
        assert!(matches!(
            &app.modal,
            Some(Modal::OwnProfile(f))
                if f.avatar_path.as_str().ends_with("avatar_")
                && f.notice.as_deref() == Some("avatar_alice.png  avatar_bob.png")
        ));
        type_text(&mut app, "a");
        app.on_event(key(KeyCode::Tab));
        assert!(matches!(
            &app.modal,
            Some(Modal::OwnProfile(f)) if f.avatar_path.as_str() == img.to_string_lossy()
        ));

        // Dropped non-image file is rejected cleanly without sending command.
        app.on_paste(&txt.to_string_lossy());
        app.on_event(key(KeyCode::Enter));
        assert!(drain_cmds(&mut commands).is_empty());
        assert!(matches!(
            &app.modal,
            Some(Modal::OwnProfile(f)) if f.error.as_deref().is_some_and(|e| e.contains("not a supported image"))
        ));

        // Dropped valid PNG file sends SetOwnAvatar and Remove avatar sends RemoveOwnAvatar.
        app.on_paste(&format!("'{}'", img.to_string_lossy()));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::SetOwnAvatar { path }] if path == &img
        ));
        app.on_matrix(MatrixEvent::OwnProfileResult {
            what: OwnProfileAction::Avatar,
            result: Ok(()),
        });

        click_on(&mut app, &mut terminal, "Remove avatar");
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::RemoveOwnAvatar]
        ));
        app.on_matrix(MatrixEvent::OwnProfileResult {
            what: OwnProfileAction::RemoveAvatar,
            result: Ok(()),
        });
        assert!(matches!(
            &app.modal,
            Some(Modal::OwnProfile(f)) if f.current_avatar.is_none()
        ));
    }

    #[test]
    fn room_own_name_and_live_sync_update_badge_timelines_and_threads() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::RoomNext);
        assert_eq!(app.room().unwrap().id, OTHER);
        app.on_matrix(MatrixEvent::Timeline {
            room_id: OTHER.into(),
            generation: app.generation,
            messages: vec![Message {
                id: "o1".into(),
                item: Some(ItemRef::Event("$o1:example.org".into())),
                sender: "alice".into(),
                sender_id: Some("@alice:example.org".into()),
                body: "in design".into(),
                day: "Today".into(),
                time: "12:02".into(),
                kind: MsgKind::Text,
                own: true,
                ..Message::default()
            }],
            reached_start: false,
            detached: None,
        });
        app.perform(Action::RoomPrev);
        assert_eq!(app.room().unwrap().id, ROOM);
        drain_cmds(&mut commands);

        // Open Room Info and edit Name in room… -> SetOwnName, empty restores default (None).
        app.on_event(key(KeyCode::F(4)));
        drain_cmds(&mut commands);
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(sample_details())),
        }));
        if let Some(Modal::RoomInfo(info)) = &mut app.modal {
            let idx = info
                .rows()
                .iter()
                .position(|r| *r == InfoRow::OwnName)
                .unwrap();
            info.selected = idx;
        }
        app.on_event(key(KeyCode::Enter));
        type_text(&mut app, "Rats Alice");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::SetOwnName { room_id, name: Some(n) })]
                if room_id == ROOM && n == "Rats Alice"
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM.into(),
            change: Change::OwnName,
            result: Ok(()),
        }));
        let mut updated_details = sample_details();
        updated_details.own_display_name = Some("Rats Alice".into());
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(updated_details)),
        }));
        app.on_event(key(KeyCode::Enter));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::Manage(ManageCmd::SetOwnName { room_id, name: None })] if room_id == ROOM
        ));
        app.on_event(key(KeyCode::Esc));

        // Open a thread rooted on our own message, then deliver live OwnProfile sync.
        app.perform(Action::SelectOlder);
        app.perform(Action::OpenThread);
        let rev_before = app.timeline_rev;

        let mut room_names = std::collections::BTreeMap::new();
        room_names.insert(ROOM.to_owned(), Some("Rats Alice".to_owned()));
        room_names.insert(OTHER.to_owned(), None);
        app.on_matrix(MatrixEvent::OwnProfile(OwnProfile {
            display_name: Some("Global Alice".to_owned()),
            avatar: None,
            room_names,
        }));

        assert!(app.timeline_rev > rev_before);
        assert_eq!(app.own_profile_hint().as_deref(), Some("Global Alice"));
        // Active thread root and restored room timeline both reflect the updated name.
        assert!(app.threads.open.is_some());
        find(&screen(&mut app, &mut terminal), "Rats Alice");
        app.perform(Action::CloseThread);
        assert!(app.threads.open.is_none());
        assert!(
            app.room()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.own && m.sender == "Rats Alice")
        );
        // Inactive room OTHER has explicit None room override, falling back to localpart.
        let other = app.rooms.iter().find(|r| r.id == OTHER).unwrap();
        assert_eq!(other.messages[0].sender, "alice");
    }

    #[test]
    fn user_info_marks_shared_rooms_maybe_more_when_incomplete() {
        let (mut app, mut terminal, mut commands) = connected();
        app.open_user_info(BOB.into(), Some(ROOM.into()), Some("Bob".into()));
        let req = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };

        // Case 1: some shared rooms found + candidates left unqueried / timed out.
        app.on_matrix(MatrixEvent::UserProfile {
            request: req,
            result: Ok(Box::new(UserProfile {
                user_id: BOB.into(),
                display_name: Some("Bob".into()),
                shared_rooms: vec![UserSharedRoom {
                    room_id: OTHER.into(),
                    name: "Design".into(),
                    direct: false,
                    invited: false,
                }],
                shared_rooms_incomplete: true,
                ..UserProfile::default()
            })),
        });
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Shared rooms (1, maybe more)"), "{text}");

        // Case 2: zero shared rooms found, yet fallback checks were incomplete.
        app.on_event(key(KeyCode::Esc));
        app.open_user_info(CAROL.into(), Some(ROOM.into()), Some("Carol".into()));
        let req2 = match drain_cmds(&mut commands).pop() {
            Some(MatrixCmd::FetchUserProfile { request, .. }) => request,
            other => panic!("expected FetchUserProfile, got {other:?}"),
        };
        app.on_matrix(MatrixEvent::UserProfile {
            request: req2,
            result: Ok(Box::new(UserProfile {
                user_id: CAROL.into(),
                display_name: Some("Carol".into()),
                shared_rooms: Vec::new(),
                shared_rooms_incomplete: true,
                ..UserProfile::default()
            })),
        });
        let text2 = screen(&mut app, &mut terminal);
        assert!(text2.contains("Shared rooms (0, maybe more)"), "{text2}");
    }
}
