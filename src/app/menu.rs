//! The context menu on `App`: opening it by right click or by key, moving in it, and
//! running what was chosen. What it offers is decided in `crate::menu`.
//!
//! The menu remembers what it is about by id. The thing is looked up again when an
//! entry is run, and a menu whose thing is gone closes (`menu_follow`).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::App;
use crate::action::{Action, Pane};
use crate::keymap::Context;
use crate::manage::{Click, InfoRow};
use crate::matrix::MatrixCmd;
use crate::menu::{self, Anchor, Menu, MessageFacts, MessageRef, Run, Target};
use crate::modal::Modal;
use crate::model::{ItemRef, RoomKind};
use crate::ui::hit::HitId;

impl App {
    fn menu_open(&self) -> bool {
        matches!(self.modal, Some(Modal::Menu(_)))
    }

    // ----- what a menu is about ------------------------------------------------------

    fn message_ref(&self, index: usize) -> Option<MessageRef> {
        let message = self.messages().get(index)?;
        Some(MessageRef {
            room_id: self.room_id()?,
            thread: self.thread_key().map(str::to_owned),
            id: message.id.clone(),
            index,
            state: message.state,
            kind: message.kind,
            on_server: matches!(message.item, Some(ItemRef::Event(_))),
        })
    }

    /// Where that message is now, if it is still among the messages shown and still
    /// what the menu was made for: one that was sent, failed or deleted meanwhile
    /// would need other entries (discarding is not deleting).
    fn find_message(&self, wanted: &MessageRef) -> Option<usize> {
        let index = self.locate_message(wanted)?;
        wanted.fits(self.messages().get(index)?).then_some(index)
    }

    fn locate_message(&self, wanted: &MessageRef) -> Option<usize> {
        if self.room_id().as_deref() != Some(wanted.room_id.as_str())
            || self.thread_key() != wanted.thread.as_deref()
        {
            return None;
        }
        if wanted.id.is_empty() {
            // Demo data has no ids, and does not move either.
            return (wanted.index < self.messages().len()).then_some(wanted.index);
        }
        self.messages().iter().position(|m| m.id == wanted.id)
    }

    /// The position of `url` among the links of the message at `index`.
    fn find_link(&self, index: usize, url: &str) -> Option<usize> {
        let message = self.messages().get(index)?;
        crate::rich::Rich::of(message.html.as_deref(), &message.body)
            .links
            .iter()
            .position(|link| link == url)
    }

    fn menu_target_alive(&self, target: &Target) -> bool {
        match target {
            Target::Message(message) => self.find_message(message).is_some(),
            Target::Link { message, url } => self
                .find_message(message)
                .is_some_and(|index| self.find_link(index, url).is_some()),
            Target::Room { id, kind } => self.find_room(id, *kind).is_some(),
            Target::Composer { room_id } => self.room_id().as_deref() == Some(room_id.as_str()),
        }
    }

    /// The room a menu is about: its index in `rooms` and its place among the rooms
    /// the list shows. `None` once it has left the list (or what the list is narrowed
    /// to), or has moved to another section, whose menu would be another one.
    fn find_room(&self, id: &str, kind: RoomKind) -> Option<(usize, usize)> {
        let index = self.rooms.iter().position(|room| room.id == id)?;
        if self.rooms[index].kind != kind {
            return None;
        }
        let at = self.visible_rooms().iter().position(|i| *i == index)?;
        Some((index, at))
    }

    /// Closes a menu about something that is no longer there or no longer what the
    /// menu was made for: another room is shown, the message was removed, sent or
    /// deleted, the room left the list or changed its section.
    pub(super) fn menu_follow(&mut self) {
        if let Some(Modal::Menu(menu)) = &self.modal
            && !self.menu_target_alive(&menu.target)
        {
            self.modal = None;
        }
    }

    // ----- opening -------------------------------------------------------------------

    fn show_menu(&mut self, target: Target, title: &str, items: Vec<menu::Item>, at: Anchor) {
        match Menu::new(target, title, items, at) {
            Some(menu) => self.open_modal(Modal::Menu(menu)),
            None => self.show_toast("Nothing to do here"),
        }
        self.dirty = true;
    }

    /// The message becomes the selected one, as with a click.
    fn select_for_menu(&mut self, index: usize) -> bool {
        if !self.selectable(index) {
            return false;
        }
        self.set_focus(Pane::Timeline);
        self.timeline.selected = Some(index);
        true
    }

    fn open_message_menu(&mut self, index: usize, at: Anchor) {
        if !self.select_for_menu(index) {
            return;
        }
        let (Some(target), Some(message)) = (self.message_ref(index), self.messages().get(index))
        else {
            return;
        };
        let event = match &message.item {
            Some(ItemRef::Event(id)) => Some(id.as_str()),
            _ => None,
        };
        let facts = MessageFacts {
            connected: self.matrix.is_some(),
            in_thread: self.thread_open(),
            read_markers: self.read_markers,
            permalink: event.is_some_and(|event| {
                menu::permalink(&target.room_id, event, self.user_id.as_deref()).is_some()
            }),
            sender_ignored: message
                .sender_id
                .as_deref()
                .is_some_and(|id| self.is_user_ignored(id)),
        };
        let items = menu::message_items(message, facts, &self.keymap);
        self.show_menu(Target::Message(target), "", items, at);
    }

    fn open_link_menu(&mut self, index: usize, link: usize, at: Anchor) {
        if !self.select_for_menu(index) {
            return;
        }
        let url = self.messages().get(index).and_then(|message| {
            crate::rich::Rich::of(message.html.as_deref(), &message.body)
                .links
                .into_iter()
                .nth(link)
        });
        let (Some(message), Some(url)) = (self.message_ref(index), url) else {
            return;
        };
        let title = url.clone();
        self.show_menu(
            Target::Link { message, url },
            &title,
            menu::link_items(),
            at,
        );
    }

    fn open_room_menu(&mut self, index: usize, at: Anchor) {
        let Some(room) = self.rooms.get(index) else {
            return;
        };
        let target = Target::Room {
            id: room.id.clone(),
            kind: room.kind,
        };
        let title = room.name.clone();
        let items = menu::room_items(room, &self.keymap);
        self.show_menu(target, &title, items, at);
    }

    fn open_composer_menu(&mut self, at: Anchor) {
        let Some(room) = self.room() else {
            return;
        };
        let target = Target::Composer {
            room_id: room.id.clone(),
        };
        let can_attach = room.kind != RoomKind::Invite
            && !matches!(self.compose_mode, super::ComposeMode::Edit(_));
        let has_draft = !self.composer.is_empty() || !self.attachments().is_empty();
        self.set_focus(Pane::Composer);
        let items = menu::composer_items(can_attach, has_draft, &self.keymap);
        self.show_menu(target, "", items, at);
    }

    /// The menu key: the menu of what has the focus, next to it.
    pub(super) fn open_menu_by_key(&mut self) {
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        match self.focus {
            Pane::Timeline => {
                let Some(index) = self.timeline.selected else {
                    self.show_toast("No message is selected");
                    return;
                };
                // Next to the message if it is on screen, else in the pane.
                let at = match self.hits.rect_of(HitId::Message(index)) {
                    Some(rect) => Anchor::below(rect, 4),
                    None => self
                        .hits
                        .rect_of(HitId::Pane(Pane::Timeline))
                        .map_or(Anchor::at(0, 0), |pane| Anchor::at(pane.x + 5, pane.y + 1)),
                };
                self.open_message_menu(index, at);
            }
            Pane::Sidebar => {
                let Some(index) = self.cursor_room() else {
                    self.show_toast("No room is selected");
                    return;
                };
                let at = self
                    .hits
                    .rect_of(HitId::Room(index))
                    .or_else(|| self.hits.rect_of(HitId::Pane(Pane::Sidebar)))
                    .map_or(Anchor::at(0, 0), |rect| Anchor::below(rect, 4));
                self.open_room_menu(index, at);
            }
            Pane::Composer => {
                let area = self.composer_view.area;
                let (column, row) = self.composer_view.cursor.unwrap_or((area.x, area.y));
                self.open_composer_menu(Anchor::at(column, row));
            }
        }
    }

    /// A right click on `hit`.
    fn open_menu_at(&mut self, hit: Option<HitId>, at: Anchor) {
        match hit {
            Some(HitId::Link(message, link)) => self.open_link_menu(message, link, at),
            Some(
                HitId::Message(index)
                | HitId::Reaction(index, _)
                | HitId::Image(index)
                | HitId::Thread(index)
                | HitId::ReplyQuote(index),
            ) => self.open_message_menu(index, at),
            Some(HitId::Room(index)) => self.open_room_menu(index, at),
            Some(HitId::ComposerText | HitId::Attachment(_) | HitId::Pane(Pane::Composer)) => {
                self.open_composer_menu(at);
            }
            _ => {}
        }
    }

    // ----- input ---------------------------------------------------------------------

    /// The menu's share of the mouse: right clicks open it, and while it is open a
    /// click runs an entry or closes it. True if the event was dealt with.
    pub(super) fn menu_mouse(&mut self, mouse: &MouseEvent, hit: Option<HitId>) -> bool {
        let open = self.menu_open();
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if open => {
                self.dirty = true;
                match hit {
                    Some(HitId::Menu(index)) => self.run_menu_at(index),
                    // Its border.
                    Some(HitId::Overlay) => {}
                    // Anywhere else closes it, and does nothing more.
                    _ => self.modal = None,
                }
                true
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if open {
                    self.modal = None;
                    self.dirty = true;
                    if matches!(hit, Some(HitId::Menu(_) | HitId::Overlay)) {
                        return true;
                    }
                } else if self.modal.is_some() || self.help_open || self.images.viewer_open() {
                    // Another popup has the screen.
                    return false;
                }
                // With mouse capture switched off, a click that was still on its way
                // is the terminal's.
                if !self.mouse {
                    return open;
                }
                self.open_menu_at(hit, Anchor::at(mouse.column, mouse.row));
                true
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if open => {
                // A menu taller than the window scrolls with its selection.
                if let Some(Modal::Menu(menu)) = &mut self.modal {
                    let last = menu.items.len() - 1;
                    let to = if mouse.kind == MouseEventKind::ScrollUp {
                        menu.selected.saturating_sub(1)
                    } else {
                        (menu.selected + 1).min(last)
                    };
                    menu.select(to);
                    self.dirty = true;
                }
                true
            }
            MouseEventKind::Moved if open => {
                if let (Some(HitId::Menu(index)), Some(Modal::Menu(menu))) = (hit, &mut self.modal)
                    && menu.selected != index
                    && index < menu.items.len()
                {
                    menu.select(index);
                    self.dirty = true;
                }
                // What lies around the menu does not light up while it is open.
                if self.hover.take().is_some() {
                    self.dirty = true;
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn on_menu_key(&mut self, key: KeyEvent, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        // The key that opened it (whatever it is bound to) closes it again.
        let context = match self.focus {
            Pane::Sidebar => Context::Sidebar,
            Pane::Timeline => Context::Timeline,
            Pane::Composer => Context::Composer,
        };
        if self.keymap.lookup(context, key) == Some(Action::ContextMenu) {
            self.modal = None;
            return;
        }
        let Some(Modal::Menu(menu)) = &mut self.modal else {
            return;
        };
        match code {
            KeyCode::Esc | KeyCode::Left => self.modal = None,
            KeyCode::Up | KeyCode::BackTab => menu.step(-1),
            KeyCode::Down | KeyCode::Tab => menu.step(1),
            KeyCode::Home | KeyCode::PageUp => menu.select(0),
            KeyCode::End | KeyCode::PageDown => menu.select(menu.items.len() - 1),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let selected = menu.selected;
                self.run_menu_at(selected);
            }
            KeyCode::Char(letter) if plain => menu.jump(letter),
            _ => {}
        }
    }

    // ----- running -------------------------------------------------------------------

    /// Runs the entry at `index` of the open menu. The menu closes first: what runs
    /// may open a popup of its own.
    pub(super) fn run_menu_at(&mut self, index: usize) {
        // An entry that asks first is only armed by the first Enter or click.
        if let Some(Modal::Menu(menu)) = &mut self.modal
            && !menu.confirmed(index)
        {
            self.dirty = true;
            return;
        }
        let Some(Modal::Menu(menu)) = self.modal.take() else {
            return;
        };
        // A line that only says something: choosing it just closes the menu.
        let Some(run) = menu.run_at(index) else {
            return;
        };
        match menu.target {
            Target::Message(wanted) => {
                let Some(index) = self.find_message(&wanted) else {
                    self.show_toast("That message has changed or is gone");
                    return;
                };
                if !self.select_for_menu(index) {
                    return;
                }
                match run {
                    Run::Do(action) => self.perform(action),
                    Run::CopyPermalink => self.copy_permalink(index),
                    Run::ReadBy => {
                        let read_by = &self.messages()[index].read_by;
                        let items = menu::read_by_items(read_by);
                        let Some(target) = self.message_ref(index) else {
                            return;
                        };
                        self.show_menu(Target::Message(target), "Read by", items, menu.anchor);
                    }
                    Run::IgnoreUser => {
                        if let Some(user_id) = self.messages()[index].sender_id.clone() {
                            self.send_ignore_user(user_id, true);
                        }
                    }
                    Run::UnignoreUser => {
                        if let Some(user_id) = self.messages()[index].sender_id.clone() {
                            self.send_ignore_user(user_id, false);
                        }
                    }
                    Run::Report => self.open_report_modal(index),
                    _ => {}
                }
            }
            Target::Link { message, url } => {
                let found = self
                    .find_message(&message)
                    .and_then(|index| Some((index, self.find_link(index, &url)?)));
                let Some((index, link)) = found else {
                    self.show_toast("That link is no longer there");
                    return;
                };
                self.select_for_menu(index);
                match run {
                    Run::OpenLink => {
                        self.open_link(index, link);
                    }
                    Run::CopyLink => {
                        self.pending_copy = Some(url);
                        self.show_toast("Link copied");
                    }
                    _ => {}
                }
            }
            Target::Room { id, kind } => {
                let Some((index, at)) = self.find_room(&id, kind) else {
                    self.show_toast("That room has changed or is no longer listed");
                    return;
                };
                match run {
                    Run::OpenRoom => self.perform(Action::OpenRoom(index)),
                    Run::MarkRead => self.mark_room_read(index),
                    Run::Notifications => self.open_info(Some(index), Some(InfoRow::Notify)),
                    Run::Do(action) => self.perform_for_room(at, action),
                    _ => {}
                }
            }
            Target::Composer { room_id } => {
                if self.room_id().as_deref() != Some(room_id.as_str()) {
                    return;
                }
                self.set_focus(Pane::Composer);
                match run {
                    Run::Do(action) => self.perform(action),
                    Run::ClearDraft => {
                        self.clear_draft();
                    }
                    _ => {}
                }
            }
        }
    }

    /// Performs a room-level action for the room at place `at` of the room list.
    /// Those actions are about the room under the list's cursor, so the cursor is
    /// put there for the moment; focus and cursor are as before afterwards.
    ///
    /// Only for the actions named here. They open a popup or send a command and
    /// touch neither focus nor cursor; one that did (opening a room, say) would have
    /// its effect undone by the restoring below, so nothing else gets through.
    fn perform_for_room(&mut self, at: usize, action: Action) {
        if !matches!(
            action,
            Action::RoomInfo | Action::LeaveRoom | Action::Manage(Click::Favourite)
        ) {
            debug_assert!(false, "not a room action of the menu: {action:?}");
            return;
        }
        let before = (self.focus, self.sidebar.cursor);
        self.focus = Pane::Sidebar;
        self.sidebar.cursor = at;
        self.perform(action);
        debug_assert_eq!((self.focus, self.sidebar.cursor), (Pane::Sidebar, at));
        (self.focus, self.sidebar.cursor) = before;
    }

    /// Tells the server that everything in that room has been read. The room need
    /// not be the open one.
    fn mark_room_read(&mut self, index: usize) {
        let public = self.read_receipts;
        let Some(room) = self.rooms.get_mut(index) else {
            return;
        };
        // Shown at once; the next room list from the server has the last word.
        room.unread = 0;
        room.bold = false;
        room.mention = false;
        if let Some(matrix) = &self.matrix {
            // About the room, also while one of its threads is open.
            matrix.send(MatrixCmd::MarkRead {
                room_id: room.id.clone(),
                public,
            });
        }
    }

    fn copy_permalink(&mut self, index: usize) {
        let link = self.messages().get(index).and_then(|message| {
            let Some(ItemRef::Event(event)) = &message.item else {
                return None;
            };
            menu::permalink(&self.room_id()?, event, self.user_id.as_deref())
        });
        match link {
            Some(link) => {
                self.pending_copy = Some(link);
                self.show_toast("Link copied");
            }
            None => self.show_toast("This message has no link"),
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::Event;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::matrix::{MatrixEvent, MatrixHandle, RoomCategory, RoomSummary};
    use crate::model::{Message, ReadBy, Reader, SendState};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Alt-↑: from the composer to the newest message, whatever the composer holds.
    fn alt_up() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT))
    }

    fn mouse(button: MouseButton, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(button),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn right(column: u16, row: u16) -> Event {
        mouse(MouseButton::Right, column, row)
    }

    fn left(column: u16, row: u16) -> Event {
        mouse(MouseButton::Left, column, row)
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    /// Column and row of the first occurrence of `needle` on the screen.
    fn find(text: &str, needle: &str) -> (u16, u16) {
        for (row, line) in text.lines().enumerate() {
            if let Some(at) = line.find(needle) {
                let column = crate::text::width(&line[..at]) as u16;
                return (column, row as u16);
            }
        }
        panic!("{needle:?} is not on screen:\n{text}");
    }

    /// Whether one line of the screen has `left` and, after it, `right`.
    fn same_line(text: &str, left: &str, right: &str) -> bool {
        text.lines().any(|line| {
            line.find(left)
                .is_some_and(|at| line[at + left.len()..].contains(right))
        })
    }

    fn demo() -> (App, Terminal<TestBackend>) {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        screen(&mut app, &mut terminal);
        (app, terminal)
    }

    fn open_menu(app: &App) -> &Menu {
        match &app.modal {
            Some(Modal::Menu(menu)) => menu,
            other => panic!("no menu: {other:?}"),
        }
    }

    fn labels(app: &App) -> Vec<String> {
        open_menu(app)
            .items
            .iter()
            .map(|item| item.label.clone())
            .collect()
    }

    /// Right-clicks on `needle` and draws the result.
    fn right_click(app: &mut App, terminal: &mut Terminal<TestBackend>, needle: &str) -> String {
        let text = screen(app, terminal);
        let (column, row) = find(&text, needle);
        app.on_event(right(column + 1, row));
        screen(app, terminal)
    }

    /// Clicks the entry `label` of the open menu.
    fn choose(app: &mut App, terminal: &mut Terminal<TestBackend>, label: &str) {
        let text = screen(app, terminal);
        let (rect, _) = open_menu(app).place(app.screen).unwrap();
        let row = text
            .lines()
            .enumerate()
            .skip(rect.y as usize)
            .take(rect.height as usize)
            .find(|(_, line)| {
                let inside: String = line
                    .chars()
                    .skip(rect.x as usize)
                    .take(rect.width as usize)
                    .collect();
                inside.contains(label)
            })
            .map(|(row, _)| row as u16)
            .unwrap_or_else(|| panic!("{label:?} is not in the menu:\n{text}"));
        app.on_event(left(rect.x + 2, row));
    }

    fn summary(id: &str, name: &str, category: RoomCategory) -> RoomSummary {
        RoomSummary {
            id: id.into(),
            name: name.into(),
            category,
            notifications: 0,
            preview: None,
            server_notifications: 0,
            unread: false,
            mention: false,
            avatar: None,
            alias: None,
        }
    }

    fn remote(id: &str, sender: &str, body: &str) -> Message {
        Message {
            id: id.into(),
            item: Some(ItemRef::Event(format!("${id}:example.org"))),
            sender: sender.into(),
            time: "12:00".into(),
            day: "Today".into(),
            body: body.into(),
            own: sender == "me",
            ..Message::default()
        }
    }

    const ROOM: &str = "!a:example.org";

    /// An account with two rooms, the first one open with `messages`.
    fn connected(
        messages: Vec<Message>,
    ) -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = crate::config::Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Alpha", RoomCategory::Room),
            summary("!b:example.org", "Beta", RoomCategory::Room),
        ]));
        timeline(&mut app, messages);
        // Without previews, so that a message's text is on screen once.
        app.room_previews = false;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        screen(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    fn timeline(app: &mut App, messages: Vec<Message>) {
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM.into(),
            generation: app.generation,
            messages,
            reached_start: true,
            detached: None,
        });
    }

    fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            seen.push(format!("{command:?}"));
        }
        seen
    }

    #[test]
    fn a_right_click_selects_the_message_and_offers_what_applies() {
        let (mut app, mut terminal) = demo();
        let own = find(&screen(&mut app, &mut terminal), "Good to know, thanks.");
        // Someone else's message: nothing to edit or delete.
        let text = right_click(&mut app, &mut terminal, "Shipping a TUI client");
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(6));
        assert_eq!(
            labels(&app),
            ["Reply", "Reply in thread", "React…", "Copy text"]
        );
        // Drawn next to the pointer, with the keys of the keymap, nothing dimmed.
        let (column, row) = find(&text, "Shipping a TUI client");
        let (rect, _) = open_menu(&app).place(app.screen).unwrap();
        assert_eq!((rect.x, rect.bottom()), (column + 1, row));
        for binding in crate::keymap::DEFAULT {
            let label = match binding.action {
                Action::Reply => "│ Reply ",
                Action::OpenThread => "│ Reply in thread ",
                Action::React => "│ React… ",
                Action::Copy => "│ Copy text ",
                _ => continue,
            };
            if binding.context == crate::keymap::Context::Timeline {
                let hint = format!(" {} │", binding.label);
                assert!(same_line(&text, label, &hint), "{label}{hint}\n{text}");
            }
        }
        insta::assert_snapshot!("menu_message", text);

        // Our own message, by right click while the first menu is open: the menu
        // moves there.
        app.on_event(right(own.0 + 30, own.1));
        let text = screen(&mut app, &mut terminal);
        assert_eq!(app.timeline.selected, Some(5));
        assert_eq!(
            labels(&app),
            [
                "Reply",
                "Reply in thread",
                "React…",
                "Edit",
                "Copy text",
                "Delete"
            ]
        );
        assert!(same_line(&text, "│ Delete ", " Del │"), "{text}");
    }

    #[test]
    fn entries_run_by_enter_and_by_click_as_their_keys_would() {
        let (mut app, mut terminal) = demo();
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        // Enter on the first entry: reply.
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.compose_mode, super::super::ComposeMode::Reply(5));
        assert_eq!(app.focus, Pane::Composer);
        app.on_event(key(KeyCode::Esc));

        // A click on Edit.
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        choose(&mut app, &mut terminal, "Edit");
        assert!(app.modal.is_none());
        assert_eq!(app.compose_mode, super::super::ComposeMode::Edit(5));
        assert_eq!(app.composer.text(), "Good to know, thanks.");
        app.on_event(key(KeyCode::Esc));

        // Down and Enter: the thread.
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(app.thread_open());
        app.perform(Action::CloseThread);

        // React opens the picker for that message; copy fills the clipboard.
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        choose(&mut app, &mut terminal, "React…");
        assert!(matches!(&app.modal, Some(Modal::Emoji(picker)) if picker.react_to.is_some()));
        app.on_event(key(KeyCode::Esc));
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        choose(&mut app, &mut terminal, "Copy text");
        assert_eq!(app.pending_copy.as_deref(), Some("Good to know, thanks."));

        // Delete, found by its letter; the letter alone does nothing.
        let count = app.messages().len();
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        app.on_event(key(KeyCode::Char('d')));
        assert_eq!(open_menu(&app).selected, 5);
        assert_eq!(app.messages().len(), count);
        // Enter asks, moving away takes the question back, Enter twice deletes.
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.messages().len(), count);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("│ Again to delete "), "{text}");
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Down));
        assert!(!screen(&mut app, &mut terminal).contains("Again to delete"));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.messages().len(), count);
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.messages().len(), count - 1);

        // By mouse the same: the first click asks, the second on that row deletes.
        // Esc after the first leaves everything as it was. The key itself asks nothing.
        let (mut app, mut terminal) = demo();
        let count = app.messages().len();
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        choose(&mut app, &mut terminal, "Delete");
        assert!(app.modal.is_some());
        assert_eq!(app.messages().len(), count);
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        choose(&mut app, &mut terminal, "Delete");
        choose(&mut app, &mut terminal, "Again to delete");
        assert!(app.modal.is_none());
        assert_eq!(app.messages().len(), count - 1);
        app.perform(Action::Focus(Pane::Timeline));
        app.timeline.selected = Some(1);
        app.rooms[0].messages[1].own = true;
        app.on_event(key(KeyCode::Delete));
        assert_eq!(app.messages().len(), count - 2);
    }

    #[test]
    fn the_key_opens_the_same_menu_for_the_selected_message() {
        let (mut app, mut terminal) = demo();
        app.on_event(alt_up());
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(5));
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Char('m')));
        let by_key = labels(&app);
        let text = screen(&mut app, &mut terminal);
        // Next to the message.
        let (_, row) = find(&text, "Good to know, thanks.");
        let (rect, _) = open_menu(&app).place(app.screen).unwrap();
        // Right below it, covering none of it.
        assert_eq!(rect.y, row + 1, "{text}");
        insta::assert_snapshot!("menu_by_key", text);
        // Esc closes it and leaves everything as it was.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.timeline.selected, Some(5));
        assert_eq!(app.focus, Pane::Timeline);

        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        assert_eq!(labels(&app), by_key);
        // The menu key closes it again, and opens it from the keymap's other entry.
        app.on_event(key(KeyCode::Menu));
        assert!(app.modal.is_none());
        app.on_event(key(KeyCode::Menu));
        assert_eq!(labels(&app), by_key);
        app.on_event(key(KeyCode::Esc));

        // In the composer the letter is typed, not taken.
        app.perform(Action::Focus(Pane::Composer));
        app.on_event(key(KeyCode::Char('m')));
        assert!(app.modal.is_none());
        assert_eq!(app.composer.text(), "m");
    }

    #[test]
    fn a_click_elsewhere_closes_it_and_does_nothing_else() {
        let (mut app, mut terminal) = demo();
        let text = right_click(&mut app, &mut terminal, "Good to know, thanks.");
        let active = app.active;
        // On a room: the room is not opened.
        let (column, row) = find(&text, "design");
        app.on_event(left(column, row));
        assert!(app.modal.is_none());
        assert_eq!(app.active, active);
        // On its own border: it stays.
        right_click(&mut app, &mut terminal, "Good to know, thanks.");
        let (rect, _) = open_menu(&app).place(app.screen).unwrap();
        app.on_event(left(rect.x, rect.y));
        assert!(app.modal.is_some());
        // A right click on nothing in particular closes it.
        app.on_event(right(99, 29));
        assert!(app.modal.is_none());
        // Nothing opens on empty space, on system lines, or behind another popup.
        app.on_event(right(99, 29));
        assert!(app.modal.is_none());
        app.perform(Action::OpenSettings);
        screen(&mut app, &mut terminal);
        app.on_event(right(50, 15));
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
    }

    #[test]
    fn it_closes_when_the_message_or_the_room_goes_away() {
        let messages = vec![remote("1", "bob", "first"), remote("2", "me", "second")];
        let (mut app, mut terminal, mut commands) = connected(messages.clone());
        right_click(&mut app, &mut terminal, "second");
        assert!(labels(&app).contains(&"Copy link to message".to_owned()));

        // Messages arrive above and below: the menu stays with its message.
        let mut more = vec![remote("0", "bob", "older")];
        more.extend(messages.clone());
        more.push(remote("3", "bob", "newer"));
        timeline(&mut app, more.clone());
        assert!(app.modal.is_some());
        screen(&mut app, &mut terminal);
        sent(&mut commands);
        // ... and acts on it, wherever it is now.
        app.on_event(key(KeyCode::End));
        assert_eq!(open_menu(&app).items.last().unwrap().label, "Delete");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands), [format!("Redact({ROOM})")]);
        assert_eq!(app.timeline.selected, Some(2));

        // The message is removed from the timeline: the menu goes with it.
        right_click(&mut app, &mut terminal, "second");
        more.retain(|message| message.id != "2");
        timeline(&mut app, more.clone());
        assert!(app.modal.is_none());

        // Another room is shown (the open one left the list).
        right_click(&mut app, &mut terminal, "newer");
        assert!(app.modal.is_some());
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            "!b:example.org",
            "Beta",
            RoomCategory::Room,
        )]));
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_menu_that_outlived_its_message_does_nothing() {
        let (mut app, mut terminal, mut commands) = connected(vec![
            remote("1", "me", "first"),
            remote("2", "me", "second"),
        ]);
        right_click(&mut app, &mut terminal, "second");
        sent(&mut commands);
        // Changed behind the menu's back, without the check that follows an update.
        app.rooms[app.active].messages.remove(1);
        app.on_event(key(KeyCode::End));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(sent(&mut commands).is_empty());
        assert_eq!(app.messages().len(), 1);
    }

    #[test]
    fn links_to_messages_and_links_in_messages() {
        let (mut app, mut terminal, _commands) = connected(vec![remote(
            "1",
            "bob",
            "see https://example.org/a and https://example.org/b",
        )]);
        right_click(&mut app, &mut terminal, "see https");
        assert!(labels(&app).contains(&"Open link".to_owned()));
        choose(&mut app, &mut terminal, "Copy link to message");
        let link = app.pending_copy.take().expect("a link");
        assert_eq!(
            link,
            crate::menu::permalink(ROOM, "$1:example.org", Some("@me:example.org")).unwrap()
        );
        assert!(link.starts_with("https://matrix.to/#/"), "{link}");
        assert!(link.ends_with("via=example.org"), "{link}");

        // On the second link itself: that link, not the first.
        let text = right_click(&mut app, &mut terminal, "https://example.org/b");
        assert_eq!(labels(&app), ["Open link", "Copy link"]);
        assert!(text.contains("╭ https://example.org/b "), "{text}");
        assert_eq!(app.timeline.selected, Some(0));
        choose(&mut app, &mut terminal, "Copy link");
        assert_eq!(
            app.pending_copy.take().as_deref(),
            Some("https://example.org/b")
        );

        // Opening, in the demo (which starts no browser).
        let (mut app, mut terminal) = demo();
        app.rooms[0].messages[6].body =
            "see https://example.org/a and https://example.org/b".into();
        app.timeline_rev += 1;
        right_click(&mut app, &mut terminal, "https://example.org/b");
        assert_eq!(labels(&app), ["Open link", "Copy link"]);
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(
            app.toast
                .as_ref()
                .is_some_and(|(text, _)| text == "Opening https://example.org/b")
        );
    }

    #[test]
    fn unsent_messages_and_readers() {
        let mut failed = remote("2", "me", "did not make it");
        failed.item = Some(ItemRef::Transaction("txn".into()));
        failed.state = SendState::Failed;
        let mut read = remote("1", "me", "seen");
        read.read_by = ReadBy {
            readers: vec![
                Reader {
                    name: "bob".into(),
                    time: "14:05".into(),
                },
                Reader {
                    name: "carol".into(),
                    time: String::new(),
                },
            ],
            total: 4,
        };
        let (mut app, mut terminal, mut commands) = connected(vec![read, failed]);
        sent(&mut commands);
        right_click(&mut app, &mut terminal, "did not make it");
        assert_eq!(labels(&app), ["Try sending again", "Copy text", "Discard"]);
        app.on_event(key(KeyCode::Enter));
        assert_eq!(sent(&mut commands), [format!("Retry({ROOM})")]);
        right_click(&mut app, &mut terminal, "did not make it");
        choose(&mut app, &mut terminal, "Discard");
        assert_eq!(sent(&mut commands), [format!("Redact({ROOM})")]);

        // Who has read it: a list in place of the menu, about the same message.
        right_click(&mut app, &mut terminal, "seen");
        choose(&mut app, &mut terminal, "Read by…");
        assert_eq!(labels(&app), ["bob", "carol", "and 2 more"]);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("╭ Read by "), "{text}");
        assert!(same_line(&text, "│ bob ", " 14:05 │"), "{text}");
        // Its lines do nothing but close it.
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn rooms_by_right_click_and_leaving_still_asks() {
        let (mut app, mut terminal, mut commands) = connected(vec![remote("1", "bob", "hello")]);
        sent(&mut commands);
        let text = right_click(&mut app, &mut terminal, "Beta");
        assert_eq!(
            labels(&app),
            [
                "Open",
                "Room info",
                "Add to favourites",
                "Notification mode…",
                "Leave…"
            ]
        );
        assert!(text.contains("╭ Beta "), "{text}");
        assert!(same_line(&text, "│ Room info ", " F4 │"), "{text}");
        // The room was not opened and the focus has not moved.
        assert_eq!(app.active, 0);
        assert_eq!(app.focus, Pane::Composer);

        // Leaving: the room info opens with its question; nothing is left yet.
        choose(&mut app, &mut terminal, "Leave…");
        let Some(Modal::RoomInfo(info)) = &app.modal else {
            panic!("no room info: {:?}", app.modal);
        };
        assert_eq!(info.room_id, "!b:example.org");
        assert_eq!(info.current(), Some(InfoRow::Leave));
        let seen = sent(&mut commands);
        assert!(
            seen.iter().all(|command| !command.contains("Leave")),
            "{seen:?}"
        );
        assert_eq!(app.focus, Pane::Composer);
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        sent(&mut commands);

        // Favourite: for that room, not for the open one.
        right_click(&mut app, &mut terminal, "Beta");
        choose(&mut app, &mut terminal, "Add to favourites");
        assert_eq!(
            sent(&mut commands),
            ["Manage(SetFavourite(!b:example.org, true))"]
        );

        // Notifications: its row in the room info.
        right_click(&mut app, &mut terminal, "Beta");
        choose(&mut app, &mut terminal, "Notification mode…");
        let Some(Modal::RoomInfo(info)) = &mut app.modal else {
            panic!("no room info: {:?}", app.modal);
        };
        assert_eq!(info.room_id, "!b:example.org");
        // The row is only there once the details are; it is selected then.
        assert_eq!(info.current(), Some(InfoRow::Favourite));
        info.set_details(crate::matrix::RoomDetails {
            notify: Some(crate::matrix::NotifyLevel::ALL[0]),
            ..Default::default()
        });
        assert_eq!(info.current(), Some(InfoRow::Notify));
        app.on_event(key(KeyCode::Esc));
        sent(&mut commands);

        // Open.
        right_click(&mut app, &mut terminal, "Beta");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.active, 1);
        assert_eq!(app.focus, Pane::Composer);
    }

    #[test]
    fn marking_a_room_as_read_is_about_that_room() {
        let (mut app, mut terminal, mut commands) = connected(vec![remote("1", "bob", "hello")]);
        let mut beta = summary("!b:example.org", "Beta", RoomCategory::Room);
        beta.unread = true;
        beta.notifications = 3;
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Alpha", RoomCategory::Room),
            beta,
        ]));
        sent(&mut commands);
        right_click(&mut app, &mut terminal, "Beta");
        choose(&mut app, &mut terminal, "Mark as read");
        assert_eq!(sent(&mut commands), ["MarkRead(!b:example.org)"]);
        assert_eq!(app.rooms[1].unread, 0);
        assert!(!app.rooms[1].bold);
        // With previews a room takes two lines; the second belongs to it as well.
        app.room_previews = true;
        app.rooms[1].preview = Some("bob: the second line".into());
        right_click(&mut app, &mut terminal, "bob: the");
        assert_eq!(
            open_menu(&app).target,
            Target::Room {
                id: "!b:example.org".into(),
                kind: RoomKind::Room,
            }
        );
        app.on_event(key(KeyCode::Esc));
        // Nothing left to mark.
        right_click(&mut app, &mut terminal, "Beta");
        assert!(!labels(&app).contains(&"Mark as read".to_owned()));
        // The room leaves the list while its menu is open.
        app.on_matrix(MatrixEvent::Rooms(vec![summary(
            ROOM,
            "Alpha",
            RoomCategory::Room,
        )]));
        assert!(app.modal.is_none());
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn a_message_that_was_sent_meanwhile_is_not_discarded_or_replied_to() {
        // On its way: "Discard" takes it out of the queue, without a question.
        let mut sending = remote("2", "me", "on its way");
        sending.item = Some(ItemRef::Transaction("txn".into()));
        sending.state = SendState::Sending;
        let first = remote("1", "bob", "first");
        let (mut app, mut terminal, mut commands) = connected(vec![first.clone(), sending]);
        sent(&mut commands);
        right_click(&mut app, &mut terminal, "on its way");
        assert_eq!(labels(&app), ["Copy text", "Discard"]);
        // The server confirms it while the menu is open: same id, now an event.
        // The menu, made for something unsent, closes.
        timeline(
            &mut app,
            vec![first.clone(), remote("2", "me", "on its way")],
        );
        assert!(app.modal.is_none());
        assert!(sent(&mut commands).is_empty());
        // Its menu now asks before deleting.
        right_click(&mut app, &mut terminal, "on its way");
        assert_eq!(labels(&app).last().unwrap(), "Delete");
        app.on_event(key(KeyCode::End));
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Esc));

        // The same change behind the menu's back (no update came through the usual
        // way): the entry is checked again when it is run, and does nothing.
        let mut failed = remote("3", "me", "did not make it");
        failed.item = Some(ItemRef::Transaction("txn3".into()));
        failed.state = SendState::Failed;
        timeline(&mut app, vec![first.clone(), failed]);
        right_click(&mut app, &mut terminal, "did not make it");
        assert_eq!(labels(&app), ["Try sending again", "Copy text", "Discard"]);
        let active = app.active;
        app.rooms[active].messages[1] = remote("3", "me", "did not make it");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        // Neither retried nor turned into a reply.
        assert!(sent(&mut commands).is_empty());
        assert_eq!(app.compose_mode, super::super::ComposeMode::Normal);
        right_click(&mut app, &mut terminal, "did not make it");
        app.rooms[active].messages[1].state = SendState::Sending;
        app.on_event(key(KeyCode::End));
        app.on_event(key(KeyCode::Enter));
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
    }

    #[test]
    fn a_message_deleted_by_someone_meanwhile_closes_its_menu() {
        let message = remote("1", "me", "soon gone");
        let (mut app, mut terminal, mut commands) = connected(vec![message.clone()]);
        sent(&mut commands);
        right_click(&mut app, &mut terminal, "soon gone");
        // Armed, even.
        app.on_event(key(KeyCode::End));
        app.on_event(key(KeyCode::Enter));
        assert_eq!(open_menu(&app).armed, Some(open_menu(&app).items.len() - 1));
        let mut deleted = message;
        deleted.kind = crate::model::MsgKind::Redacted;
        deleted.body = "Message deleted".into();
        timeline(&mut app, vec![deleted]);
        assert!(app.modal.is_none());
        app.on_event(key(KeyCode::Enter));
        let seen = sent(&mut commands);
        assert!(
            seen.iter().all(|command| !command.contains("Redact")),
            "{seen:?}"
        );
        assert!(app.pending_copy.is_none());
    }

    #[test]
    fn a_room_that_changed_or_left_the_list_closes_its_menu() {
        let (mut app, mut terminal, mut commands) = connected(vec![remote("1", "bob", "hello")]);
        let rooms = |beta: RoomCategory| {
            MatrixEvent::Rooms(vec![
                summary(ROOM, "Alpha", RoomCategory::Room),
                summary("!b:example.org", "Beta", beta),
            ])
        };
        // A favourite stops being one elsewhere while "Remove from favourites" is
        // on offer: the menu closes instead of adding it again.
        app.on_matrix(rooms(RoomCategory::Favourite));
        right_click(&mut app, &mut terminal, "Beta");
        assert!(labels(&app).contains(&"Remove from favourites".to_owned()));
        sent(&mut commands);
        app.on_matrix(rooms(RoomCategory::Room));
        assert!(app.modal.is_none());
        // Behind the menu's back: checked again when run.
        app.on_matrix(rooms(RoomCategory::Favourite));
        right_click(&mut app, &mut terminal, "Beta");
        app.rooms[1].kind = RoomKind::Room;
        choose(&mut app, &mut terminal, "Remove from favourites");
        assert!(app.modal.is_none());
        assert!(sent(&mut commands).is_empty());

        // The list is narrowed to something the room is not part of (here by the
        // filter; a space does the same): the menu closes with the next update,
        // and an entry run before that does nothing rather than half of its work.
        right_click(&mut app, &mut terminal, "Beta");
        app.sidebar.filter = "alp".into();
        app.on_matrix(rooms(RoomCategory::Room));
        assert!(app.modal.is_none());
        app.sidebar.filter.clear();
        right_click(&mut app, &mut terminal, "Beta");
        app.sidebar.filter = "alp".into();
        let before = (app.focus, app.sidebar.cursor, app.active);
        for entry in ["Room info", "Open"] {
            if app.modal.is_none() {
                app.sidebar.filter.clear();
                right_click(&mut app, &mut terminal, "Beta");
                app.sidebar.filter = "alp".into();
            }
            let at = labels(&app)
                .iter()
                .position(|label| label == entry)
                .unwrap();
            app.run_menu_at(at);
            assert!(app.modal.is_none(), "{entry}");
            assert_eq!(
                (app.focus, app.sidebar.cursor, app.active),
                before,
                "{entry}"
            );
        }
        assert!(sent(&mut commands).is_empty());

        // Listed: focus and cursor are where they were after a room action.
        app.sidebar.filter.clear();
        screen(&mut app, &mut terminal);
        let before = (app.focus, app.sidebar.cursor);
        right_click(&mut app, &mut terminal, "Beta");
        choose(&mut app, &mut terminal, "Room info");
        assert!(matches!(app.modal, Some(Modal::RoomInfo(_))));
        assert_eq!((app.focus, app.sidebar.cursor), before);
    }

    #[test]
    fn the_bound_key_closes_it_the_wheel_scrolls_it_and_moving_selects() {
        // Rebound: the new key opens and closes; the old one is just a letter.
        let (mut app, mut terminal) = demo();
        let (code, mods) = crate::keymap::parse_key(Context::Timeline, "x").unwrap();
        app.keymap
            .apply_rebind(Context::Timeline, Action::ContextMenu, code, mods);
        app.on_event(alt_up());
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Char('m')));
        assert!(app.modal.is_none());
        app.on_event(key(KeyCode::Char('x')));
        assert!(app.modal.is_some());
        app.on_event(key(KeyCode::Char('m')));
        assert!(app.modal.is_some());
        app.on_event(key(KeyCode::Char('x')));
        assert!(app.modal.is_none());

        // A menu taller than the window: the wheel walks through it, without
        // going round and without scrolling the messages behind it.
        let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Up));
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Char('x')));
        let count = open_menu(&app).items.len();
        let (rect, first) = open_menu(&app).place(app.screen).unwrap();
        assert!((rect.height as usize) < count + 2 && first == 0);
        let scroll = app.timeline.scroll;
        app.on_event(mouse_event(MouseEventKind::ScrollUp, 5, 2));
        assert_eq!(open_menu(&app).selected, 0);
        for _ in 0..count + 3 {
            app.on_event(mouse_event(MouseEventKind::ScrollDown, 5, 2));
        }
        assert_eq!(open_menu(&app).selected, count - 1);
        assert_eq!(app.timeline.scroll, scroll);
        let text = screen(&mut app, &mut terminal);
        let (rect, first) = open_menu(&app).place(app.screen).unwrap();
        assert!(first > 0);
        // What is shown now can be clicked: the last entry, Delete, gets armed.
        let last = labels(&app).pop().unwrap();
        assert_eq!(last, "Delete");
        assert!(text.contains("│ Delete "), "{text}");
        app.on_event(left(rect.x + 2, rect.bottom() - 2));
        assert_eq!(open_menu(&app).armed, Some(count - 1));
        // Moving the pointer to another entry selects it and takes the question
        // back; what lies around the menu does not light up.
        app.on_event(mouse_event(MouseEventKind::Moved, rect.x + 2, rect.y + 1));
        assert_eq!(open_menu(&app).selected, first);
        assert_eq!(open_menu(&app).armed, None);
        app.on_event(mouse_event(MouseEventKind::Moved, 0, 0));
        assert_eq!(app.hover, None);
        // A right click on the menu itself only closes it.
        app.on_event(right(rect.x + 2, rect.y + 1));
        assert!(app.modal.is_none());
    }

    #[test]
    fn who_has_read_is_not_offered_when_read_markers_are_off() {
        let mut read = remote("1", "me", "seen");
        read.read_by = ReadBy {
            readers: vec![Reader {
                name: "bob".into(),
                time: "14:05".into(),
            }],
            total: 1,
        };
        let (mut app, mut terminal, _commands) = connected(vec![read]);
        right_click(&mut app, &mut terminal, "seen");
        assert!(labels(&app).contains(&"Read by…".to_owned()));
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Setting(crate::modal::Setting::ReadMarkers));
        right_click(&mut app, &mut terminal, "seen");
        assert!(!labels(&app).contains(&"Read by…".to_owned()));
        app.on_event(key(KeyCode::Esc));

        // Both display settings can be reached from the command palette.
        for (typed, was) in [("read markers", false), ("room previews", false)] {
            app.perform(Action::OpenPalette);
            for c in typed.chars() {
                app.on_event(key(KeyCode::Char(c)));
            }
            let text = screen(&mut app, &mut terminal);
            assert!(text.to_lowercase().contains(typed), "{text}");
            app.on_event(key(KeyCode::Enter));
            assert!(app.modal.is_none());
            let now = if typed == "read markers" {
                app.read_markers
            } else {
                app.room_previews
            };
            assert_eq!(now, !was, "{typed}");
        }
    }

    #[test]
    fn menus_in_threads_on_invitations_and_on_other_parts_of_a_message() {
        // In a thread: no thread entry; the menu goes when the thread is left.
        let (mut app, mut terminal) = demo();
        app.perform(Action::OpenRoom(1));
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Up));
        app.perform(Action::OpenThread);
        assert!(app.thread_open());
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Up));
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Char('m')));
        assert!(labels(&app).iter().all(|label| !label.contains("thread")));
        let Some(Modal::Menu(menu)) = app.modal.clone() else {
            panic!("no menu");
        };
        app.modal = None;
        app.perform(Action::CloseThread);
        assert!(!app.menu_target_alive(&menu.target));

        // A reaction and a thread line belong to their message.
        let (mut app, mut terminal) = demo();
        let text = screen(&mut app, &mut terminal);
        let (column, row) = find(&text, "👍");
        app.on_event(right(column, row));
        assert_eq!(app.timeline.selected, Some(1));
        assert!(labels(&app).contains(&"React…".to_owned()));
        // The reaction itself was not toggled.
        assert_eq!(app.rooms[0].messages[1].reactions[0].count, 2);
        app.on_event(key(KeyCode::Esc));
        // A section heading of the room list has no menu.
        let (column, row) = find(&text, "People");
        app.on_event(right(column, row));
        assert!(app.modal.is_none());

        // An invitation: little to do with it, and declining asks in its room info.
        let (mut app, mut terminal, mut commands) = connected(Vec::new());
        app.on_matrix(MatrixEvent::Rooms(vec![
            summary(ROOM, "Alpha", RoomCategory::Room),
            summary("!i:example.org", "Party", RoomCategory::Invite),
        ]));
        sent(&mut commands);
        right_click(&mut app, &mut terminal, "Party");
        assert_eq!(labels(&app), ["Open", "Room info", "Decline invitation…"]);
        choose(&mut app, &mut terminal, "Decline invitation…");
        assert!(
            matches!(&app.modal, Some(Modal::RoomInfo(info)) if info.room_id == "!i:example.org")
        );
        let seen = sent(&mut commands);
        assert!(
            seen.iter().all(|command| !command.contains("Leave")),
            "{seen:?}"
        );
    }

    #[test]
    fn the_rooms_receipts_do_not_follow_a_message_into_its_thread_and_the_setting_is_saved() {
        let (mut app, mut terminal) = demo();
        app.perform(Action::OpenRoom(1));
        app.rooms[1].messages[0].read_by = ReadBy {
            readers: vec![Reader {
                name: "bob".into(),
                time: String::new(),
            }],
            total: 1,
        };
        app.timeline_rev += 1;
        assert!(screen(&mut app, &mut terminal).contains("✓"));
        app.on_event(key(KeyCode::Up));
        app.perform(Action::OpenThread);
        assert!(app.thread_open());
        assert!(app.messages()[0].read_by.is_empty());
        assert!(!screen(&mut app, &mut terminal).contains("✓"));
        // Back in the room they are there again.
        app.perform(Action::CloseThread);
        assert!(screen(&mut app, &mut terminal).contains("✓"));

        // The settings row writes `read_markers` to the configuration.
        let dir = std::env::temp_dir().join(format!("matrix-rat-markers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("config.toml");
        app.config_file = Some(file.clone());
        app.perform(Action::Setting(crate::modal::Setting::ReadMarkers));
        assert!(!crate::config::Config::load(&file).unwrap().read_markers);
        assert!(!screen(&mut app, &mut terminal).contains("✓"));
        app.perform(Action::Setting(crate::modal::Setting::ReadMarkers));
        assert!(crate::config::Config::load(&file).unwrap().read_markers);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_room_list_by_key() {
        let (mut app, mut terminal) = demo();
        app.perform(Action::Focus(Pane::Sidebar));
        app.on_event(key(KeyCode::Down));
        screen(&mut app, &mut terminal);
        let room = app.cursor_room().unwrap();
        app.on_event(key(KeyCode::Menu));
        assert_eq!(
            open_menu(&app).target,
            Target::Room {
                id: app.rooms[room].id.clone(),
                kind: app.rooms[room].kind,
            }
        );
        // Room info, as F4 would show it for the room under the cursor.
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        let Some(Modal::RoomInfo(info)) = &app.modal else {
            panic!("no room info: {:?}", app.modal);
        };
        assert_eq!(info.room_id, app.rooms[room].id);
    }

    #[test]
    fn the_composer_offers_files_emoji_and_clearing() {
        let (mut app, mut terminal) = demo();
        let area = app.composer_view.area;
        app.on_event(right(area.x + 1, area.y));
        assert_eq!(labels(&app), ["Attach file…", "Emoji…"]);
        assert_eq!(app.focus, Pane::Composer);
        let text = screen(&mut app, &mut terminal);
        assert!(same_line(&text, "│ Attach file… ", " Alt-A │"), "{text}");
        assert!(same_line(&text, "│ Emoji… ", " Alt-E │"), "{text}");
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(&app.modal, Some(Modal::Emoji(picker)) if picker.react_to.is_none()));
        app.on_event(key(KeyCode::Esc));

        for c in "draft".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Menu));
        assert_eq!(labels(&app), ["Attach file…", "Emoji…", "Clear draft"]);
        choose(&mut app, &mut terminal, "Clear draft");
        assert!(app.composer.is_empty());
        assert!(app.modal.is_none());

        app.on_event(key(KeyCode::Menu));
        app.on_event(key(KeyCode::Enter));
        assert!(app.composer.text().starts_with("/upload"));
    }

    #[test]
    fn it_stays_on_screen_at_the_edges_and_in_tiny_windows() {
        for (width, height) in [
            (100, 30),
            (60, 12),
            (40, 8),
            (24, 6),
            (12, 4),
            (8, 3),
            (5, 2),
        ] {
            let mut app = App::demo();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            screen(&mut app, &mut terminal);
            app.on_event(alt_up());
            screen(&mut app, &mut terminal);
            app.on_event(key(KeyCode::Char('m')));
            assert!(app.modal.is_some(), "{width}x{height}");
            for anchor in [
                (0, 0),
                (width - 1, 0),
                (0, height - 1),
                (width - 1, height - 1),
            ] {
                if let Some(Modal::Menu(menu)) = &mut app.modal {
                    menu.anchor = Anchor::at(anchor.0, anchor.1);
                }
                // Drawing must not panic, whatever the size; and every entry can
                // be reached.
                for _ in 0..8 {
                    screen(&mut app, &mut terminal);
                    app.on_event(key(KeyCode::Down));
                }
                if let Some((rect, _)) = open_menu(&app).place(app.screen) {
                    assert_eq!(app.screen.intersection(rect), rect, "{width}x{height}");
                }
            }
            // A click in the corner is on the menu or closes it; never a panic.
            app.on_event(left(width - 1, height - 1));
            screen(&mut app, &mut terminal);
        }
        // At the right edge of a normal window the whole menu is visible.
        let (mut app, mut terminal) = demo();
        app.on_event(alt_up());
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Char('m')));
        if let Some(Modal::Menu(menu)) = &mut app.modal {
            menu.anchor = Anchor::at(99, 29);
        }
        let text = screen(&mut app, &mut terminal);
        for label in labels(&app) {
            assert!(text.contains(&label), "{label}:\n{text}");
        }
        insta::assert_snapshot!("menu_corner", text);
    }

    #[test]
    fn without_mouse_capture_only_the_key_opens_it() {
        // With the setting off the terminal sends no clicks at all; should one
        // arrive anyway (the switch is still on its way), it is not acted on twice
        // and the keyboard path is unaffected.
        let (mut app, mut terminal) = demo();
        app.perform(Action::Setting(crate::modal::Setting::Mouse));
        assert!(!app.mouse);
        let text = screen(&mut app, &mut terminal);
        let (column, row) = find(&text, "Good to know, thanks.");
        app.on_event(right(column, row));
        assert!(app.modal.is_none());
        app.on_event(alt_up());
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Char('m')));
        assert!(app.modal.is_some());
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.focus, Pane::Composer);
    }
}
