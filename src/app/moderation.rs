//! Moderation actions, pinned messages popup, and message forwarding on `App`.

use std::collections::HashSet;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use super::rooms::NO_ACCOUNT;
use crate::action::Pane;
use crate::lineedit::EditKeys;
use crate::matrix::{ManageCmd, MatrixCmd};
use crate::modal::{Modal, PinnedPopup, ReasonConfirmKind, ReasonConfirmPopup};
use crate::model::{ItemRef, Message, MsgKind, PinnedMessage, ReadBy, Room, SendState};
use crate::palette::{ForwardTarget, Palette};
use crate::text::sanitize_line;

impl App {
    pub(super) fn can_redact_message(room: &Room, message: &Message) -> bool {
        if message.is_system_or_marker()
            || message.kind == MsgKind::Redacted
            || message.state != SendState::Sent
        {
            return false;
        }
        message.own || room.power.can_redact_other
    }

    pub(super) fn can_pin_message(room: &Room, message: &Message) -> bool {
        !message.is_system_or_marker()
            && message.kind != MsgKind::Redacted
            && message.state == SendState::Sent
            && room.power.can_pin
            && (message.event_id().is_some() || !message.id.is_empty())
    }

    pub(super) fn can_forward_message(message: &Message) -> bool {
        !message.is_system_or_marker()
            && message.kind != MsgKind::Redacted
            && message.kind != MsgKind::Undecryptable
            && message.state == SendState::Sent
    }

    pub(super) fn can_ban_sender(
        room: &Room,
        message: &Message,
        self_user: Option<&str>,
    ) -> Option<String> {
        if message.is_system_or_marker() || message.own {
            return None;
        }
        let sender_id = message.sender_id.as_ref()?;
        if self_user == Some(sender_id.as_str()) {
            return None;
        }
        room.power
            .can_ban_user(self_user, sender_id)
            .then(|| sender_id.clone())
    }

    pub(super) fn open_redact_confirm(&mut self, index: usize) {
        let Some(room) = self.room() else {
            return;
        };
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if message.is_system_or_marker()
            || message.kind == MsgKind::Redacted
            || message.state != SendState::Sent
        {
            return;
        }
        if !message.own && !room.power.can_redact_other {
            self.show_toast("You do not have permission to delete other people's messages");
            return;
        }
        let Some(item) = message.item.clone().or_else(|| {
            (!message.id.is_empty() && self.matrix.is_none())
                .then(|| ItemRef::Event(message.id.clone()))
        }) else {
            return;
        };
        let room_id = room.id.clone();
        let sender = sanitize_line(&message.sender);
        let preview = sanitize_line(&message.body);
        self.open_modal(Modal::ReasonConfirm(ReasonConfirmPopup::redact(
            room_id, item, sender, preview,
        )));
    }

    pub(super) fn open_ban_confirm_from_message(&mut self, index: usize) {
        let Some(room) = self.room() else {
            return;
        };
        let Some(message) = self.messages().get(index) else {
            return;
        };
        let Some(user_id) = Self::can_ban_sender(room, message, self.user_id.as_deref()) else {
            self.show_toast("You cannot ban that member");
            return;
        };
        let room_id = room.id.clone();
        let display_name = sanitize_line(&message.sender);
        self.open_modal(Modal::ReasonConfirm(ReasonConfirmPopup::ban(
            room_id,
            user_id,
            display_name,
        )));
    }

    pub(super) fn on_reason_confirm_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::ReasonConfirm(popup)) = &mut self.modal else {
            return;
        };
        if mods == KeyModifiers::NONE && code == KeyCode::Esc {
            self.modal = None;
            return;
        }
        if mods == KeyModifiers::NONE && code == KeyCode::Tab {
            popup.step(1);
            return;
        }
        if code == KeyCode::BackTab {
            popup.step(-1);
            return;
        }
        if mods == KeyModifiers::NONE
            && matches!(code, KeyCode::Left | KeyCode::Right)
            && !popup.on_reason()
        {
            popup.flip_button();
            return;
        }
        if mods == KeyModifiers::NONE && code == KeyCode::Enter {
            if popup.on_cancel() {
                self.modal = None;
            } else {
                self.submit_reason_confirm();
            }
            return;
        }
        if popup.on_reason() {
            popup.reason.handle_key(code, mods, EditKeys::FULL);
        }
    }

    pub(super) fn on_reason_confirm_paste(&mut self, first_line: &str) {
        if let Some(Modal::ReasonConfirm(popup)) = &mut self.modal {
            popup.focus_reason();
            popup.reason.paste(first_line);
        }
    }

    pub(super) fn submit_reason_confirm(&mut self) {
        let Some(Modal::ReasonConfirm(popup)) = self.modal.take() else {
            return;
        };
        let reason = {
            let trimmed = sanitize_line(popup.reason.as_str()).trim().to_owned();
            (!trimmed.is_empty()).then_some(trimmed)
        };
        match popup.kind {
            ReasonConfirmKind::Redact { room_id, item, .. } => {
                if self.matrix.is_some() {
                    self.send_matrix(MatrixCmd::Redact {
                        room_id,
                        item,
                        reason,
                    });
                } else if self.room_id().as_deref() == Some(room_id.as_str())
                    && let Some(idx) = self.messages().iter().position(|m| {
                        m.item.as_ref() == Some(&item)
                            || matches!(&item, ItemRef::Event(id) if id == &m.id)
                    })
                {
                    if let Some(messages) = self.messages_mut() {
                        messages.remove(idx);
                    }
                    self.timeline.selected = self
                        .find_selectable(idx, false)
                        .or_else(|| self.find_selectable(idx, true));
                    self.timeline.reveal_selected = true;
                    if self.timeline.selected.is_none() {
                        self.set_focus(Pane::Composer);
                    }
                    self.show_toast("Message deleted");
                }
            }
            ReasonConfirmKind::Ban {
                room_id, user_id, ..
            } => {
                if self.matrix.is_none() {
                    self.show_toast(NO_ACCOUNT);
                    return;
                }
                self.send_matrix(MatrixCmd::Manage(ManageCmd::Ban {
                    room_id,
                    user_id,
                    reason,
                }));
            }
        }
    }

    pub(super) fn toggle_pin_message(&mut self, index: Option<usize>) {
        let Some(index) = index.or(self.timeline.selected) else {
            return;
        };
        let Some(room) = self.room() else {
            return;
        };
        if !room.power.can_pin {
            self.show_toast("You do not have permission to pin messages");
            return;
        }
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if !Self::can_pin_message(room, message) {
            self.show_toast("This message cannot be pinned");
            return;
        }
        let room_id = room.id.clone();
        let next_pinned = !message.pinned;
        if self.matrix.is_some() {
            let Some(event_id) = message.event_id().map(str::to_owned) else {
                self.show_toast("This message has no event ID yet");
                return;
            };
            self.send_matrix(MatrixCmd::Pin {
                room_id,
                event_id,
                pinned: next_pinned,
            });
            return;
        }
        // Demo mode: toggle pin locally on room + open thread without disturbing unread state.
        let msg_id = message
            .event_id()
            .map_or_else(|| message.id.clone(), str::to_owned);
        self.apply_demo_pin(&room_id, &msg_id, next_pinned);
        self.show_toast(if next_pinned {
            "Message pinned"
        } else {
            "Message unpinned"
        });
    }

    fn apply_demo_pin(&mut self, room_id: &str, event_id: &str, pinned: bool) {
        if let Some(room) = self.rooms.iter_mut().find(|r| r.id == room_id) {
            if pinned {
                if !room.pinned_ids.iter().any(|id| id == event_id) {
                    room.pinned_ids.push(event_id.to_owned());
                }
            } else {
                room.pinned_ids.retain(|id| id != event_id);
            }
            for msg in &mut room.messages {
                if msg.id == event_id || msg.event_id() == Some(event_id) {
                    msg.pinned = pinned;
                }
            }
        }
        self.sync_open_thread_pinned_flags(room_id);
    }

    pub(super) fn open_pinned(&mut self) {
        let Some(room) = self.room() else {
            return;
        };
        if room.is_pending() {
            self.show_toast(room.kind.pending_hint());
            return;
        }
        let room_id = room.id.clone();
        let room_name = room.name.clone();
        let can_pin = room.power.can_pin;
        if self.matrix.is_some() {
            self.open_modal(Modal::Pinned(PinnedPopup::open(
                room_id.clone(),
                room_name,
                can_pin,
            )));
            self.send_matrix(MatrixCmd::FetchPinned { room_id });
            return;
        }
        let items = self.collect_demo_pinned(&room_id);
        let mut popup = PinnedPopup::open(room_id, room_name, can_pin);
        popup.set_items(items, None);
        self.open_modal(Modal::Pinned(popup));
    }

    fn collect_demo_pinned(&self, room_id: &str) -> Vec<PinnedMessage> {
        let Some(room) = self.rooms.iter().find(|r| r.id == room_id) else {
            return Vec::new();
        };
        let mut items = Vec::new();
        for msg in &room.messages {
            if !msg.pinned || msg.kind == MsgKind::Redacted {
                continue;
            }
            let ignored = msg
                .sender_id
                .as_deref()
                .is_some_and(|id| self.is_user_ignored(id));
            if ignored {
                continue;
            }
            items.push(PinnedMessage {
                event_id: msg.event_id().map_or_else(|| msg.id.clone(), str::to_owned),
                sender: msg.sender.clone(),
                sender_id: msg.sender_id.clone(),
                day: msg.day.clone(),
                time: msg.time.clone(),
                snippet: sanitize_line(&msg.body),
                redacted: false,
                undecryptable: false,
                ignored: false,
            });
        }
        items
    }

    pub(super) fn on_pinned_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match (code, mods) {
            (KeyCode::Esc, KeyModifiers::NONE) => self.modal = None,
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if let Some(Modal::Pinned(popup)) = &mut self.modal {
                    popup.step(-1);
                }
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if let Some(Modal::Pinned(popup)) = &mut self.modal {
                    popup.step(1);
                }
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                if let Some(Modal::Pinned(popup)) = &mut self.modal {
                    popup.step(-5);
                }
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                if let Some(Modal::Pinned(popup)) = &mut self.modal {
                    popup.step(5);
                }
            }
            (KeyCode::Home, KeyModifiers::NONE) => {
                if let Some(Modal::Pinned(popup)) = &mut self.modal {
                    popup.select_first();
                }
            }
            (KeyCode::End, KeyModifiers::NONE) => {
                if let Some(Modal::Pinned(popup)) = &mut self.modal {
                    popup.select_last();
                }
            }
            (KeyCode::Enter, KeyModifiers::NONE) => self.activate_pinned_selected(),
            (KeyCode::Delete, KeyModifiers::NONE) | (KeyCode::Char('u'), KeyModifiers::NONE) => {
                self.unpin_pinned_selected()
            }
            _ => {}
        }
    }

    pub(super) fn scroll_pinned(&mut self, delta: isize) {
        if let Some(Modal::Pinned(popup)) = &mut self.modal {
            popup.scroll_by(delta);
        }
    }

    pub(super) fn activate_pinned_at(&mut self, index: usize) {
        if let Some(Modal::Pinned(popup)) = &mut self.modal
            && index < popup.items.len()
        {
            popup.selected = index;
        }
        self.activate_pinned_selected();
    }

    fn activate_pinned_selected(&mut self) {
        let Some(Modal::Pinned(popup)) = &self.modal else {
            return;
        };
        let Some(item) = popup.selected_item() else {
            return;
        };
        let room_id = popup.room_id.clone();
        let event_id = item.event_id.clone();
        self.modal = None;
        self.jump_to_event(room_id, event_id, None);
    }

    pub(super) fn unpin_pinned_at(&mut self, index: usize) {
        if let Some(Modal::Pinned(popup)) = &mut self.modal
            && index < popup.items.len()
        {
            popup.selected = index;
        }
        self.unpin_pinned_selected();
    }

    fn unpin_pinned_selected(&mut self) {
        let Some(Modal::Pinned(popup)) = &mut self.modal else {
            return;
        };
        if !popup.can_pin {
            return;
        }
        let Some(item) = popup.selected_item() else {
            return;
        };
        let room_id = popup.room_id.clone();
        let event_id = item.event_id.clone();
        if self.matrix.is_some() {
            self.send_matrix(MatrixCmd::Pin {
                room_id,
                event_id,
                pinned: false,
            });
            return;
        }
        popup.remove_event(&event_id);
        self.apply_demo_pin(&room_id, &event_id, false);
        self.show_toast("Message unpinned");
    }

    fn apply_pinned_set(messages: &mut [Message], pinned_set: &HashSet<&str>, fallback_id: bool) {
        for message in messages {
            message.pinned = message
                .event_id()
                .or_else(|| (fallback_id && !message.id.is_empty()).then_some(message.id.as_str()))
                .is_some_and(|id| pinned_set.contains(id));
        }
    }

    pub(super) fn on_pinned_events(&mut self, room_id: String, pinned_ids: Vec<String>) {
        let pinned_set: HashSet<&str> = pinned_ids.iter().map(String::as_str).collect();
        if let Some(room) = self.rooms.iter_mut().find(|r| r.id == room_id) {
            Self::apply_pinned_set(&mut room.messages, &pinned_set, false);
            room.pinned_ids = pinned_ids;
        }
        self.sync_open_thread_pinned_flags(&room_id);
        if let Some(Modal::Pinned(popup)) = &self.modal
            && popup.room_id == room_id
            && self.matrix.is_some()
        {
            self.send_matrix(MatrixCmd::FetchPinned { room_id });
        }
    }

    pub(super) fn on_pinned_list(
        &mut self,
        room_id: String,
        mut items: Vec<PinnedMessage>,
        error: Option<String>,
    ) {
        for item in &mut items {
            if item
                .sender_id
                .as_deref()
                .is_some_and(|uid| self.ignored_users.contains(uid))
            {
                item.sender = "Ignored user".to_owned();
                item.sender_id = None;
                item.snippet = "Message from an ignored user".to_owned();
                item.ignored = true;
            }
        }
        let can_pin = self
            .rooms
            .iter()
            .find(|r| r.id == room_id)
            .is_some_and(|r| r.power.can_pin);
        if let Some(Modal::Pinned(popup)) = &mut self.modal
            && popup.room_id == room_id
        {
            popup.can_pin = can_pin;
            popup.set_items(items, error);
        }
    }

    pub(super) fn sync_pinned_flags(&mut self, room_id: &str) {
        let Some(pinned_ids) = self
            .rooms
            .iter()
            .find(|r| r.id == room_id)
            .map(|r| r.pinned_ids.clone())
        else {
            return;
        };
        let pinned_set: HashSet<&str> = pinned_ids.iter().map(String::as_str).collect();
        let fallback_id = self.matrix.is_none();
        if let Some(room) = self.rooms.iter_mut().find(|r| r.id == room_id) {
            Self::apply_pinned_set(&mut room.messages, &pinned_set, fallback_id);
        }
        self.sync_open_thread_pinned_flags(room_id);
    }

    fn sync_open_thread_pinned_flags(&mut self, room_id: &str) {
        let Some(pinned_ids) = self
            .rooms
            .iter()
            .find(|r| r.id == room_id)
            .map(|r| r.pinned_ids.clone())
        else {
            return;
        };
        if self.room_id().as_deref() != Some(room_id) {
            return;
        }
        let pinned_set: HashSet<&str> = pinned_ids.iter().map(String::as_str).collect();
        let fallback_id = self.matrix.is_none();
        if let Some(thread) = &mut self.threads.open {
            thread.apply_pinned(&pinned_set, fallback_id);
        }
    }

    pub(super) fn open_forward_picker(&mut self, index: Option<usize>) {
        let Some(index) = index.or(self.timeline.selected) else {
            return;
        };
        let Some(room) = self.room() else {
            return;
        };
        let Some(message) = self.messages().get(index) else {
            return;
        };
        if !Self::can_forward_message(message) {
            let reason = if message.kind == MsgKind::Redacted {
                "Deleted messages cannot be forwarded"
            } else if message.state != SendState::Sent {
                "Unsent messages cannot be forwarded"
            } else if message.kind == MsgKind::Undecryptable {
                "Encrypted messages that cannot be decrypted cannot be forwarded"
            } else {
                "This message cannot be forwarded"
            };
            self.show_toast(reason);
            return;
        }
        let from_room_id = room.id.clone();
        let event_id = message
            .event_id()
            .map(str::to_owned)
            .unwrap_or_else(|| message.id.clone());
        if self.matrix.is_some() && message.event_id().is_none() {
            self.show_toast("This message cannot be forwarded yet");
            return;
        }
        let preview = sanitize_line(&message.body);
        let preview = if preview.is_empty() {
            sanitize_line(&message.sender)
        } else {
            preview
        };
        let ctx = self.palette_context();
        let target = ForwardTarget {
            from_room_id,
            event_id,
            preview,
        };
        self.open_modal(Modal::Palette(Palette::open_forward(&ctx.rooms, target)));
    }

    pub(super) fn forward_to_room(&mut self, target: ForwardTarget, to_room_id: String) {
        if self.matrix.is_some() {
            self.send_matrix(MatrixCmd::Forward {
                from_room_id: target.from_room_id,
                event_id: target.event_id,
                to_room_id,
            });
            return;
        }
        let source = self
            .rooms
            .iter()
            .find(|r| r.id == target.from_room_id)
            .and_then(|r| {
                r.messages.iter().find(|m| {
                    m.id == target.event_id || m.event_id() == Some(target.event_id.as_str())
                })
            })
            .cloned()
            .or_else(|| {
                self.threads
                    .open
                    .as_ref()
                    .and_then(|t| t.find_message(&target.event_id).cloned())
            });
        let Some(mut copy) = source else {
            self.show_toast("Message is no longer available");
            return;
        };
        if !Self::can_forward_message(&copy) {
            self.show_toast("This message cannot be forwarded");
            return;
        }
        copy.id = format!("fwd-{}", self.next_request());
        copy.item = Some(ItemRef::Event(copy.id.clone()));
        copy.sender = "You".to_owned();
        copy.sender_id = self.user_id.clone();
        copy.own = true;
        copy.time = "now".to_owned();
        copy.state = SendState::Sent;
        copy.edited = false;
        copy.pinned = false;
        copy.reply_to = None;
        copy.reply_event = None;
        copy.thread = None;
        copy.reactions.clear();
        copy.read_by = ReadBy::default();
        copy.shield = None;
        copy.mentions = None;
        if let Some(dest) = self.rooms.iter_mut().find(|r| r.id == to_room_id) {
            let preview = sanitize_line(&copy.body);
            dest.preview = (!preview.is_empty()).then_some(preview);
            dest.messages.push(copy);
            self.show_forward_toast(to_room_id, String::new());
        }
    }

    pub(super) fn on_forward_result(
        &mut self,
        to_room_id: String,
        to_room_name: String,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => self.show_forward_toast(to_room_id, to_room_name),
            Err(err) => self.show_toast(format!("Forward failed: {err}")),
        }
    }

    pub(super) fn show_forward_toast(&mut self, room_id: String, fallback_name: String) {
        let name = self
            .rooms
            .iter()
            .find(|r| r.id == room_id)
            .map(|r| sanitize_line(&r.name))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                let fallback = sanitize_line(&fallback_name);
                if fallback.is_empty() {
                    room_id.clone()
                } else {
                    fallback
                }
            });
        let now = Instant::now();
        self.toast = Some((format!("Forwarded to {name}"), now));
        self.toast_room = Some((room_id, now));
    }

    pub(super) fn click_forward_toast(&mut self) {
        let Some((room_id, _)) = self.toast_room.take() else {
            return;
        };
        self.toast = None;
        let Some(index) = self.find_joined_room(&room_id) else {
            self.show_toast("That room is no longer available");
            return;
        };
        if self.space.is_some() && !self.space_rooms.contains(&room_id) {
            self.select_space(None);
        }
        self.threads.open = None;
        self.open_room(index);
        if let Some(at) = self.visible_rooms().iter().position(|i| *i == index) {
            self.sidebar.cursor = at;
            self.sidebar.reveal_cursor = true;
        }
        self.set_focus(Pane::Composer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::matrix::{
        Change, ManageCmd, ManageEvent, MatrixEvent, MatrixHandle, MemberRole, RoomDetails,
        RoomMember,
    };
    use crate::model::{MsgKind, RoomPower, SendState};
    use crate::ui;
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    const ROOM_A: &str = "!a:example.org";
    const ROOM_B: &str = "!b:example.org";
    const ALICE: &str = "@alice:example.org";
    const BOB: &str = "@bob:example.org";
    const CAROL: &str = "@carol:example.org";

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn click(col: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
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
                let col = crate::text::width(&line[..byte]);
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{text}");
    }

    fn drain_cmds(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = commands.try_recv() {
            out.push(cmd);
        }
        out
    }

    fn summary(id: &str, name: &str) -> crate::matrix::RoomSummary {
        crate::matrix::RoomSummary {
            encrypted: false,
            id: id.into(),
            name: name.into(),
            category: crate::matrix::RoomCategory::Room,
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

    fn sample_msg(id: &str, sender: &str, sender_id: &str, body: &str, own: bool) -> Message {
        Message {
            id: id.into(),
            item: Some(ItemRef::Event(format!("${id}:example.org"))),
            sender: sender.into(),
            sender_id: Some(sender_id.into()),
            body: body.into(),
            day: "Today".into(),
            time: "12:00".into(),
            kind: MsgKind::Text,
            own,
            ..Message::default()
        }
    }

    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: ALICE.into(),
        });
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM_A, "Rats"),
            summary(ROOM_B, "Design"),
        ]));
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM_A.into(),
            generation: app.generation,
            messages: vec![
                sample_msg("e1", "bob", BOB, "hello from bob", false),
                sample_msg("e2", "alice", ALICE, "own message", true),
            ],
            reached_start: false,
            detached: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal, commands)
    }

    fn power(redact: bool, ban: bool, unban: bool, pin: bool, roles: bool) -> RoomPower {
        let mut member_levels = std::collections::HashMap::new();
        member_levels.insert(ALICE.into(), 100);
        member_levels.insert(BOB.into(), 0);
        member_levels.insert(CAROL.into(), 50);
        RoomPower {
            own_level: 100,
            own_is_creator: false,
            can_redact_other: redact,
            can_ban: ban,
            can_unban: unban,
            can_kick: ban,
            can_pin: pin,
            can_change_roles: roles,
            can_rename: roles,
            can_set_topic: roles,
            users_default: 0,
            creators: Vec::new(),
            member_levels,
        }
    }

    fn menu_labels(app: &App) -> Vec<&str> {
        let Some(Modal::Menu(menu)) = &app.modal else {
            return Vec::new();
        };
        menu.items.iter().map(|it| it.label.as_str()).collect()
    }

    #[test]
    fn redact_permissions_reason_popup_own_message_and_unread_line() {
        let (mut app, mut terminal, mut commands) = connected();
        drain_cmds(&mut commands);

        // Place an unread ReadMarker above bob's message so we can prove redaction keeps it.
        app.unread.settle_immediately();
        app.begin_unread_line();
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM_A.into(),
            generation: app.generation,
            messages: vec![
                Message {
                    id: "marker".into(),
                    kind: MsgKind::ReadMarker,
                    ..Message::default()
                },
                sample_msg("e1", "bob", BOB, "hello from bob", false),
                sample_msg("e2", "alice", ALICE, "own message", true),
            ],
            reached_start: false,
            detached: None,
        });
        assert!(app.messages().iter().any(|m| m.kind == MsgKind::ReadMarker));

        // Without can_redact_other: bob's menu hides Delete… and Del does nothing.
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "e1");
        app.perform(Action::ContextMenu);
        assert!(!menu_labels(&app).iter().any(|l| l.starts_with("Delete")));
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Delete);
        assert!(app.modal.is_none());
        assert!(drain_cmds(&mut commands).is_empty());

        // Own message still uses menu double-confirm ("Delete" -> "Again to delete") without ReasonConfirmPopup.
        app.perform(Action::SelectNewer);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "e2");
        app.perform(Action::ContextMenu);
        assert!(menu_labels(&app).contains(&"Delete"));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Delete");
        app.on_event(click(col, row));
        assert!(drain_cmds(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Again to delete");
        app.on_event(click(col, row));
        assert!(app.modal.is_none());
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Redact { room_id, item: ItemRef::Event(id), reason: None }]
                if room_id == ROOM_A && id == "$e2:example.org"
        ));

        // Grant moderation redact power: bob's message menu shows "Delete…" and opens ReasonConfirmPopup.
        app.on_matrix(MatrixEvent::RoomPower {
            room_id: ROOM_A.into(),
            power: power(true, false, false, false, false),
        });
        app.perform(Action::SelectOlder);
        assert_eq!(app.messages()[app.timeline.selected.unwrap()].id, "e1");
        app.perform(Action::ContextMenu);
        let labels = menu_labels(&app);
        assert!(labels.contains(&"Delete…"), "{labels:?}");
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Delete…");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::ReasonConfirm(_))));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Delete message");
        type_text(&mut app, "off-topic spam");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Redact {
                room_id,
                item: ItemRef::Event(id),
                reason: Some(r),
            }] if room_id == ROOM_A && id == "$e1:example.org" && r == "off-topic spam"
        ));

        // Replacing e2 (below ReadMarker above e1) with Redacted leaves ReadMarker in place and removes Delete/Forward.
        let mut redacted = sample_msg("e2", "bob", BOB, "", false);
        redacted.kind = MsgKind::Redacted;
        redacted.item = None;
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM_A.into(),
            generation: app.generation,
            messages: vec![
                sample_msg("e1", "bob", BOB, "hello from bob", false),
                redacted,
            ],
            reached_start: false,
            detached: None,
        });
        assert!(app.messages().iter().any(|m| m.kind == MsgKind::ReadMarker));
        assert!(!App::can_forward_message(&app.messages()[2]));
    }

    #[test]
    fn room_info_ban_unban_role_chooser_and_message_menu_ban() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::RoomPower {
            room_id: ROOM_A.into(),
            power: power(false, true, true, false, true),
        });
        drain_cmds(&mut commands);

        // Message menu offers "Ban user…" for bob and opens ReasonConfirmPopup::ban.
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        app.perform(Action::ContextMenu);
        assert!(menu_labels(&app).contains(&"Ban user…"));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Ban user…");
        app.on_event(click(col, row));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Ban user");
        type_text(&mut app, "harassment");
        app.on_event(key(KeyCode::Enter));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Manage(ManageCmd::Ban {
                room_id,
                user_id,
                reason: Some(r),
            })] if room_id == ROOM_A && user_id == BOB && r == "harassment"
        ));

        // Open Room Info with alice (sole admin), bob (default, bannable, role_editable), carol (mod, not bannable), eve (banned, unbannable).
        app.on_event(key(KeyCode::F(4)));
        drain_cmds(&mut commands);
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM_A.into(),
            result: Ok(Box::new(RoomDetails {
                name: "Rats".into(),
                topic: "Topic".into(),
                joined: 3,
                invited: 0,
                own_power_level: 100,
                own_is_creator: false,
                admin_count: 1,
                members: vec![
                    RoomMember {
                        user_id: ALICE.into(),
                        name: "alice".into(),
                        role: MemberRole::Admin,
                        power_level: 100,
                        own: true,
                        role_editable: true,
                        ..RoomMember::default()
                    },
                    RoomMember {
                        user_id: CAROL.into(),
                        name: "carol".into(),
                        role: MemberRole::Moderator,
                        power_level: 75,
                        own: false,
                        removable: false,
                        bannable: false,
                        role_editable: true,
                        ..RoomMember::default()
                    },
                    RoomMember {
                        user_id: BOB.into(),
                        name: "bob".into(),
                        role: MemberRole::User,
                        power_level: 0,
                        own: false,
                        removable: true,
                        bannable: true,
                        role_editable: true,
                        ..RoomMember::default()
                    },
                    RoomMember {
                        user_id: "@eve:example.org".into(),
                        name: "eve".into(),
                        role: MemberRole::User,
                        power_level: 0,
                        banned: true,
                        unbannable: true,
                        ..RoomMember::default()
                    },
                ],
                ..RoomDetails::default()
            })),
        }));

        // Verify custom power level `75` and `Banned` section render cleanly (`unban` pill shows once selected).
        let text = screen(&mut app, &mut terminal);
        find(&text, "75");
        find(&text, "Banned");
        find(&text, "banned");

        // Filter to carol (not bannable): Alt-B shows an error and does not open BanPrompt.
        type_text(&mut app, "carol");
        app.on_event(key_mod(KeyCode::Char('b'), KeyModifiers::ALT));
        let text = screen(&mut app, &mut terminal);
        find(&text, "not allowed to ban");

        // Switch filter to bob: Alt-B opens inline BanPrompt, Esc closes prompt while staying in RoomInfo.
        for _ in 0..5 {
            app.on_event(key(KeyCode::Backspace));
        }
        type_text(&mut app, "bob");
        app.on_event(key_mod(KeyCode::Char('b'), KeyModifiers::ALT));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Ban bob");
        app.on_event(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::RoomInfo(_))));
        app.on_event(key_mod(KeyCode::Char('b'), KeyModifiers::ALT));
        type_text(&mut app, "flooding");
        app.on_event(key(KeyCode::Enter));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Manage(ManageCmd::Ban { room_id, user_id, reason: Some(r) })]
                if room_id == ROOM_A && user_id == BOB && r == "flooding"
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM_A.into(),
            change: Change::Ban,
            result: Ok(()),
        }));

        // RoleChooser on alice (sole admin): demoting self to Moderator requires a second Enter confirm.
        for _ in 0..3 {
            app.on_event(key(KeyCode::Backspace));
        }
        type_text(&mut app, "alice");
        app.on_event(key_mod(KeyCode::Char('r'), KeyModifiers::ALT));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Change role for alice");
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(drain_cmds(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Last admin!");
        app.on_event(key(KeyCode::Enter));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Manage(ManageCmd::SetRole { room_id, user_id, level: 50 })]
                if room_id == ROOM_A && user_id == ALICE
        ));
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Done {
            room_id: ROOM_A.into(),
            change: Change::SetRole,
            result: Ok(()),
        }));

        // Filter to banned eve: `unban` pill shows on the selected row, Del twice triggers ManageCmd::Unban.
        for _ in 0..5 {
            app.on_event(key(KeyCode::Backspace));
        }
        type_text(&mut app, "eve");
        let text = screen(&mut app, &mut terminal);
        find(&text, "unban");
        app.on_event(key(KeyCode::Delete));
        assert!(drain_cmds(&mut commands).is_empty());
        app.on_event(key(KeyCode::Delete));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Manage(ManageCmd::Unban { room_id, user_id, reason: None })]
                if room_id == ROOM_A && user_id == "@eve:example.org"
        ));
    }

    #[test]
    fn pins_permissions_badge_popup_jump_unpin_redacted_ignored_and_unread_line() {
        let (mut app, mut terminal, mut commands) = connected();
        drain_cmds(&mut commands);

        // Place ReadMarker first so we can verify PinnedEvents leaves it intact.
        app.unread.settle_immediately();
        app.begin_unread_line();
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM_A.into(),
            generation: app.generation,
            messages: vec![
                Message {
                    id: "marker".into(),
                    kind: MsgKind::ReadMarker,
                    ..Message::default()
                },
                sample_msg("e1", "bob", BOB, "important note", false),
            ],
            reached_start: false,
            detached: None,
        });

        // Without can_pin: menu has no "Pin", Del/u in PinnedPopup does not emit Pin.
        app.perform(Action::SelectOlder);
        app.perform(Action::ContextMenu);
        assert!(!menu_labels(&app).contains(&"Pin"));
        app.on_event(key(KeyCode::Esc));

        // Grant pin permission and press PinMessage.
        app.on_matrix(MatrixEvent::RoomPower {
            room_id: ROOM_A.into(),
            power: power(false, false, false, true, false),
        });
        app.perform(Action::PinMessage);
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Pin { room_id, event_id, pinned: true }]
                if room_id == ROOM_A && event_id == "$e1:example.org"
        ));

        // Receive PinnedEvents: timeline shows "(pinned)" and ReadMarker stays intact.
        app.on_matrix(MatrixEvent::PinnedEvents {
            room_id: ROOM_A.into(),
            ids: vec!["$e1:example.org".into()],
        });
        assert!(app.messages().iter().any(|m| m.kind == MsgKind::ReadMarker));
        assert!(app.messages()[1].pinned);
        let text = screen(&mut app, &mut terminal);
        find(&text, "(pinned)");

        // Press P (`Action::OpenPinned`) -> opens PinnedPopup and requests FetchPinned.
        app.perform(Action::OpenPinned);
        assert!(matches!(app.modal, Some(Modal::Pinned(_))));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::FetchPinned { room_id }] if room_id == ROOM_A
        ));

        // Populate PinnedList with normal pin, ignored-sender pin, and unloaded deleted pin.
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![CAROL.into()]));
        app.on_matrix(MatrixEvent::PinnedList {
            room_id: ROOM_A.into(),
            items: vec![
                crate::model::PinnedMessage {
                    event_id: "$e1:example.org".into(),
                    sender: "bob".into(),
                    sender_id: Some(BOB.into()),
                    day: "Today".into(),
                    time: "12:00".into(),
                    snippet: "important note".into(),
                    redacted: false,
                    undecryptable: false,
                    ignored: false,
                },
                crate::model::PinnedMessage {
                    event_id: "$ignored:example.org".into(),
                    sender: "carol".into(),
                    sender_id: Some(CAROL.into()),
                    day: "Today".into(),
                    time: "12:01".into(),
                    snippet: "noise from carol".into(),
                    redacted: false,
                    undecryptable: false,
                    ignored: false,
                },
                crate::model::PinnedMessage {
                    event_id: "$old_del:example.org".into(),
                    sender: "bob".into(),
                    sender_id: Some(BOB.into()),
                    day: "Yesterday".into(),
                    time: "09:00".into(),
                    snippet: String::new(),
                    redacted: true,
                    undecryptable: false,
                    ignored: false,
                },
            ],
            error: None,
        });
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("noise from carol"));
        assert!(!text.contains("carol"));
        find(&text, "Message from an ignored user");
        find(&text, "important note");
        find(&text, "[message deleted]");

        // Enter on loaded e1 jumps directly without OpenEvent; Enter on unloaded event requests OpenEvent.
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.timeline.selected, Some(1));
        assert!(drain_cmds(&mut commands).is_empty());

        app.perform(Action::OpenPinned);
        app.on_matrix(MatrixEvent::PinnedList {
            room_id: ROOM_A.into(),
            items: vec![crate::model::PinnedMessage {
                event_id: "$remote:example.org".into(),
                sender: "bob".into(),
                sender_id: Some(BOB.into()),
                day: "Yesterday".into(),
                time: "09:00".into(),
                snippet: "far away".into(),
                redacted: false,
                undecryptable: false,
                ignored: false,
            }],
            error: None,
        });
        drain_cmds(&mut commands);
        app.on_event(key(KeyCode::Char('u')));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Pin { room_id, event_id, pinned: false }]
                if room_id == ROOM_A && event_id == "$remote:example.org"
        ));

        app.perform(Action::OpenPinned);
        app.on_matrix(MatrixEvent::PinnedList {
            room_id: ROOM_A.into(),
            items: vec![crate::model::PinnedMessage {
                event_id: "$remote:example.org".into(),
                sender: "bob".into(),
                sender_id: Some(BOB.into()),
                day: "Yesterday".into(),
                time: "09:00".into(),
                snippet: "far away".into(),
                redacted: false,
                undecryptable: false,
                ignored: false,
            }],
            error: None,
        });
        drain_cmds(&mut commands);
        app.on_event(key(KeyCode::Enter));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::OpenEvent { room_id, event_id, thread_root: None, .. }]
                if room_id == ROOM_A && event_id == "$remote:example.org"
        ));
    }

    #[test]
    fn forward_guards_picker_execution_and_toast_open_from_filtered_or_thread_view() {
        let (mut app, mut terminal, mut commands) = connected();
        drain_cmds(&mut commands);

        // Undecryptable, redacted, unsent, and system messages cannot be forwarded.
        let mut utd = sample_msg("u", "bob", BOB, "", false);
        utd.kind = MsgKind::Undecryptable;
        let mut unsent = sample_msg("s", "alice", ALICE, "pending", true);
        unsent.state = SendState::Sending;
        let mut sys = sample_msg("y", "", "", "bob joined", false);
        sys.kind = MsgKind::System;
        assert!(!App::can_forward_message(&utd));
        assert!(!App::can_forward_message(&unsent));
        assert!(!App::can_forward_message(&sys));

        // Select bob's message and open forward picker.
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        app.perform(Action::ForwardMessage);
        let text = screen(&mut app, &mut terminal);
        find(&text, "Design");
        type_text(&mut app, "Design");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::Forward {
                from_room_id,
                event_id,
                to_room_id,
            }] if from_room_id == ROOM_A && event_id == "$e1:example.org" && to_room_id == ROOM_B
        ));

        // Put user in a Space filter where ROOM_B isn't listed and open a thread before clicking Open ->.
        app.spaces = vec![crate::matrix::SpaceSummary {
            id: "!space:example.org".into(),
            name: "Space".into(),
            level: 0,
            rooms: vec![ROOM_A.into()],
        }];
        app.select_space(Some(0));
        app.open_thread(0);
        assert!(app.threads.open.is_some());

        app.on_matrix(MatrixEvent::ForwardResult {
            to_room_id: ROOM_B.into(),
            to_room_name: "Design".into(),
            result: Ok(()),
        });
        let text = screen(&mut app, &mut terminal);
        find(&text, "Forwarded to Design");
        let (col, row) = find(&text, "Open →");
        app.on_event(click(col, row));

        assert_eq!(app.rooms[app.active].id, ROOM_B);
        assert!(app.space.is_none());
        assert!(app.threads.open.is_none());
    }
}
