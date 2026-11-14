//! Command palette integration on `App`: opening, input, execution, and tests.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::action::Action;
use crate::lineedit::{EditKeys, LineEdit};
use crate::modal::{Modal, Setting, SettingsMenu};
use crate::model::{MsgKind, RoomKind, SendState};
use crate::palette::{MessageCaps, Palette, PaletteContext, RoomItem, Target};
use crate::theme::Theme;

impl App {
    /// Opens the command palette populated with actions, rooms, settings, and themes.
    pub(super) fn open_palette(&mut self) {
        if self.modal.is_some() || self.images.viewer_open() {
            return;
        }
        self.theme_names = Theme::available(&self.themes_dir);
        let rooms = crate::model::RoomKind::SECTIONS
            .iter()
            .flat_map(|kind| {
                self.rooms
                    .iter()
                    .enumerate()
                    .filter(move |(_, r)| r.kind == *kind)
                    .map(|(i, r)| RoomItem {
                        id: r.id.clone(),
                        name: r.name.clone(),
                        unread: r.unread,
                        active: i == self.active,
                    })
            })
            .collect();

        let in_invite = self.room().is_some_and(|r| r.kind == RoomKind::Invite);
        let thread_open = self.thread_open();
        let selected = self.timeline.selected.and_then(|i| self.messages().get(i));
        let selection = match selected {
            Some(m) => {
                let has_links = !crate::rich::Rich::of(m.html.as_deref(), &m.body)
                    .links
                    .is_empty();
                MessageCaps {
                    has_selection: true,
                    own: m.own,
                    can_edit: m.editable(),
                    can_copy: !m.body.is_empty(),
                    can_save: self.can_save(),
                    can_open: m.image.is_some() || m.file.is_some() || has_links,
                    can_open_thread: !thread_open
                        && !in_invite
                        && !matches!(m.kind, MsgKind::System | MsgKind::ReadMarker)
                        && (self.matrix.is_none()
                            || matches!(m.item, Some(crate::model::ItemRef::Event(_)))),
                    failed_retry: self.matrix.is_some() && m.state == SendState::Failed,
                }
            }
            None => MessageCaps::default(),
        };

        let ctx = PaletteContext {
            focus: self.focus,
            compose_mode: self.compose_mode,
            in_invite,
            thread_open,
            has_room: self.room().is_some(),
            has_messages: !self.messages().is_empty(),
            open_unread: self
                .room()
                .filter(|room| room.kind != RoomKind::Invite)
                .map(|room| room.shows_unread()),
            connected: self.matrix.is_some(),
            user_id: self.user_id.clone(),
            mouse: self.mouse,
            typing_notices: self.typing_notices,
            read_receipts: self.read_receipts,
            read_markers: self.read_markers,
            room_previews: self.room_previews,
            notifications: self.notify_mode != crate::notify::NotifyMode::Off,
            download_dir: crate::files::display(&self.download_dir()),
            verify_status: crate::verify::setting_text(self.verify.security),
            wanted_theme: self.wanted_theme.clone(),
            theme_dark: self.theme_dark.clone(),
            theme_light: self.theme_light.clone(),
            themes: self.theme_names.clone(),
            rooms,
            selection,
            target: self.manage_target(),
            keymap: self.keymap.clone(),
        };
        self.open_modal(Modal::Palette(Palette::open(ctx)));
    }

    pub(super) fn on_palette_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Palette(palette)) = &mut self.modal else {
            return;
        };
        let page = palette.rows.max(1) as isize;
        match (code, mods) {
            (KeyCode::Esc, _) => self.modal = None,
            (KeyCode::Char('p'), m) if m == KeyModifiers::CONTROL => self.modal = None,
            (KeyCode::Up, _) => palette.step(-1),
            (KeyCode::Down, _) => palette.step(1),
            (KeyCode::Char('p'), m) if m == KeyModifiers::ALT => palette.step(-1),
            (KeyCode::Char('n'), m) if m == KeyModifiers::CONTROL => palette.step(1),
            (KeyCode::PageUp, _) => palette.step(-page),
            (KeyCode::PageDown, _) => palette.step(page),
            (KeyCode::Enter, _) => {
                let selected = palette.selected;
                self.run_palette_at(selected);
            }
            _ => {
                if palette
                    .query
                    .handle_key(code, mods, EditKeys::FULL)
                    .changed()
                {
                    palette.on_query_changed();
                }
            }
        }
    }

    pub(super) fn run_palette_at(&mut self, filtered_index: usize) {
        let entry = match &self.modal {
            Some(Modal::Palette(palette)) => palette.entry_at(filtered_index).cloned(),
            _ => None,
        };
        let Some(entry) = entry else {
            return;
        };
        if !entry.enabled {
            return;
        }
        // Close before dispatching so actions that open another modal (Settings, Emoji,
        // Verify, Logout confirmation) are not blocked by `self.modal.is_none()`.
        self.modal = None;
        match entry.target {
            Target::Action(action) => self.perform(action),
            Target::Room(room_id) => {
                if let Some(index) = self.rooms.iter().position(|r| r.id == room_id) {
                    self.perform(Action::OpenRoom(index));
                } else {
                    self.show_toast("That room is no longer available");
                }
            }
            Target::Theme(name) => {
                self.theme_names = Theme::available(&self.themes_dir);
                if let Some(index) = self.theme_names.iter().position(|n| n == &name) {
                    self.perform(Action::SelectTheme(index));
                } else {
                    self.show_toast(format!("Theme {name} is no longer available"));
                }
            }
        }
    }

    pub(super) fn perform_setting(&mut self, setting: Setting) {
        match setting {
            Setting::Mouse
            | Setting::TypingNotices
            | Setting::ReadReceipts
            | Setting::ReadMarkers
            | Setting::RoomPreviews
            | Setting::Notifications => {
                self.change_setting(setting, 0);
                self.show_toast(format!(
                    "{}: {}",
                    setting.label(),
                    self.setting_value(setting)
                ));
            }
            Setting::DownloadDir => {
                let mut menu = SettingsMenu::select(Setting::DownloadDir, self.has_theme_pair());
                menu.editing = Some(LineEdit::from(self.download_dir.as_str()));
                self.open_modal(Modal::Settings(menu));
            }
            Setting::Keys => self.change_setting(Setting::Keys, 0),
            Setting::Encryption => self.start_verification(),
            Setting::Logout => {
                if self.matrix.is_none() {
                    self.show_toast("The demo has no account to log out of");
                    return;
                }
                // Select Log out in Settings without arming confirm_logout yet so the
                // existing two-step confirmation applies and a held/auto-repeated Enter
                // cannot confirm logout immediately.
                self.open_modal(Modal::Settings(SettingsMenu::select(
                    Setting::Logout,
                    self.has_theme_pair(),
                )));
            }
            Setting::ThemePairing => {
                if self.has_theme_pair() {
                    self.change_setting(setting, 0);
                } else {
                    self.open_modal(Modal::Settings(SettingsMenu::select(
                        Setting::ThemeDark,
                        true,
                    )));
                }
            }
            Setting::Theme | Setting::ThemeDark | Setting::ThemeLight | Setting::SidebarWidth => {
                self.change_setting(setting, 1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Pane;
    use crate::app::{ComposeMode, ui};
    use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(code, mods))
    }

    /// Alt-↑: from the composer to the newest message, whatever the composer holds.
    fn alt_up() -> Event {
        key_mod(KeyCode::Up, KeyModifiers::ALT)
    }

    fn ctrl_p() -> Event {
        key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL)
    }

    fn click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn drawn() -> (App, Terminal<TestBackend>) {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal)
    }

    fn redraw(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
    }

    fn find(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let screen = format!("{}", terminal.backend());
        for (row, line) in screen.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                let col = crate::text::width(&line[..byte]) - 1;
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{screen}");
    }

    #[test]
    fn open_filter_run_and_esc() {
        let (mut app, mut terminal) = drawn();
        // Ctrl-P opens the palette.
        app.on_event(ctrl_p());
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        redraw(&mut app, &mut terminal);

        // Esc closes cleanly without running anything.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert_eq!(app.wanted_theme, "rat-dark");

        // Re-open, filter for a theme, and apply it with Enter.
        app.on_event(ctrl_p());
        type_text(&mut app, "gruvbox-dark");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.wanted_theme, "gruvbox-dark");

        // Toggle typing notices via palette and observe feedback toast.
        let before = app.typing_notices;
        app.on_event(ctrl_p());
        type_text(&mut app, "typing");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.typing_notices, !before);
    }

    #[test]
    fn open_and_run_by_mouse_click() {
        let (mut app, mut terminal) = drawn();
        let (col, row) = find(&terminal, "palette");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        type_text(&mut app, "bob");
        redraw(&mut app, &mut terminal);

        let (col, row) = find(&terminal, "Go to bob");
        app.on_event(click(col, row));
        assert!(app.modal.is_none());
        assert_eq!(app.rooms[app.active].name, "bob");
    }

    #[test]
    fn backdrop_click_closes_palette_without_clicking_underneath() {
        let (mut app, mut terminal) = drawn();
        app.on_event(ctrl_p());
        redraw(&mut app, &mut terminal);
        app.on_event(click(2, 2));
        assert!(app.modal.is_none());
        assert_eq!(app.active, 0);
        assert_eq!(app.focus, Pane::Composer);
    }

    #[test]
    fn disabled_entries_cannot_be_executed() {
        let (mut app, mut terminal) = drawn();
        // Composer is focused, so no message is selected and Reply is disabled.
        app.on_event(ctrl_p());
        type_text(&mut app, "reply");
        redraw(&mut app, &mut terminal);

        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        assert_eq!(app.compose_mode, ComposeMode::Normal);

        let (col, row) = find(&terminal, "Reply to selected message");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
    }

    #[test]
    fn room_jump_preserves_unsent_drafts() {
        let (mut app, _terminal) = drawn();
        type_text(&mut app, "unfinished rust draft");
        assert_eq!(app.active, 0);

        app.on_event(ctrl_p());
        type_text(&mut app, "design");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.rooms[app.active].name, "design");
        assert!(app.composer.is_empty());
        assert_eq!(app.rooms[0].draft, "unfinished rust draft");

        type_text(&mut app, "design note");
        app.on_event(ctrl_p());
        type_text(&mut app, "rust");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[app.active].name, "rust");
        assert_eq!(app.composer.text(), "unfinished rust draft");
        let design = app.rooms.iter().position(|r| r.name == "design").unwrap();
        assert_eq!(app.rooms[design].draft, "design note");
    }

    #[test]
    fn settings_actions_open_expected_flows() {
        let (mut app, mut terminal) = drawn();
        app.on_event(ctrl_p());
        type_text(&mut app, "download folder");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.modal,
            Some(Modal::Settings(menu)) if menu.editing.is_some()
        ));

        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());

        // React requires a selected message; when selected it opens the Emoji reaction picker.
        app.on_event(alt_up());
        app.on_event(ctrl_p());
        type_text(&mut app, "react");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.modal,
            Some(Modal::Emoji(p)) if p.react_to.is_some()
        ));

        app.on_event(key(KeyCode::Esc));
        // Log out selects the Settings row WITHOUT arming confirm_logout; first Enter
        // arms it without logging out, second Enter dispatches MatrixCmd::Logout.
        let (handle, mut rx) = crate::matrix::MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.user_id = Some("@alice:example.org".into());
        app.on_event(ctrl_p());
        type_text(&mut app, "log out");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.modal,
            Some(Modal::Settings(menu)) if menu.current() == Setting::Logout && !menu.confirm_logout
        ));
        assert!(rx.try_recv().is_err());

        // First Enter only arms confirmation; no Logout command sent.
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(
            &app.modal,
            Some(Modal::Settings(menu)) if menu.confirm_logout
        ));
        assert!(rx.try_recv().is_err());

        // Second Enter confirms and sends MatrixCmd::Logout.
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::matrix::MatrixCmd::Logout)
        ));
        redraw(&mut app, &mut terminal);
    }

    #[test]
    fn notifications_attach_save_thread_and_index_actions() {
        let (mut app, mut terminal) = drawn();
        // Toggle notifications from the palette.
        let before_mode = app.notify_mode;
        app.on_event(ctrl_p());
        type_text(&mut app, "notifications");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_ne!(app.notify_mode, before_mode);

        // Attach via palette puts `/upload ` into composer and is disabled while editing.
        app.on_event(ctrl_p());
        type_text(&mut app, "attach a file");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.composer.text(), "/upload ");
        app.composer.take();

        let own = app.rooms[app.active]
            .messages
            .iter()
            .rposition(|m| m.own && m.image.is_none())
            .expect("demo room has an editable own message");
        app.timeline.selected = Some(own);
        app.focus = Pane::Timeline;
        app.on_event(key(KeyCode::Char('e')));
        assert!(matches!(app.compose_mode, ComposeMode::Edit(_)));
        let edited_before = app.composer.text().to_owned();
        app.on_event(ctrl_p());
        type_text(&mut app, "attach a file");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        assert_eq!(app.composer.text(), edited_before);
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::CancelCompose);

        // Save is disabled when nothing saveable is selected, and enabled on file messages.
        app.timeline.selected = Some(own);
        app.focus = Pane::Timeline;
        assert!(!app.can_save());
        app.on_event(ctrl_p());
        type_text(&mut app, "save file");
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Palette(_))));
        app.on_event(key(KeyCode::Esc));

        let selected = app.timeline.selected.unwrap();
        let key_url = "mxc://example.org/spec".to_owned();
        app.rooms[app.active].messages[selected].file = Some(crate::model::Attachment {
            key: key_url.clone(),
            name: "spec.pdf".into(),
            size: Some(128),
            source: Box::new(matrix_sdk::ruma::events::room::MediaSource::Plain(
                matrix_sdk::ruma::OwnedMxcUri::from(key_url),
            )),
        });
        assert!(app.can_save());
        app.on_event(ctrl_p());
        type_text(&mut app, "save file");
        let enabled = match &app.modal {
            Some(Modal::Palette(p)) => p.entry_at(p.selected).is_some_and(|e| e.enabled),
            _ => unreachable!(),
        };
        assert!(enabled);
        app.on_event(key(KeyCode::Esc));

        // Out-of-range Action::OpenRoom / Action::SelectTheme are harmless no-ops.
        let active_before = app.active;
        let theme_before = app.wanted_theme.clone();
        app.perform(Action::OpenRoom(999));
        app.perform(Action::SelectTheme(999));
        assert_eq!(app.active, active_before);
        assert_eq!(app.wanted_theme, theme_before);
        redraw(&mut app, &mut terminal);
    }

    #[test]
    fn room_jump_from_open_thread_closes_thread_without_leaking_draft() {
        let (mut app, mut terminal) = drawn();
        type_text(&mut app, "rust room draft");

        // Select a message and open its thread via the command palette.
        app.on_event(key(KeyCode::Up));
        app.on_event(ctrl_p());
        type_text(&mut app, "open the message's thread");
        app.on_event(key(KeyCode::Enter));
        assert!(app.thread_open());
        assert!(app.composer.is_empty());

        // Type inside the thread, then switch rooms using the palette.
        type_text(&mut app, "thread secret draft");
        app.on_event(ctrl_p());
        type_text(&mut app, "design");
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert!(!app.thread_open());
        assert_eq!(app.rooms[app.active].name, "design");
        assert!(app.composer.is_empty());

        // Returning to the original room keeps its room draft intact and isolates the thread draft.
        app.on_event(ctrl_p());
        type_text(&mut app, "rust");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.rooms[app.active].name, "rust");
        assert_eq!(app.composer.text(), "rust room draft");

        // CloseThread via palette closes an open thread back to the room.
        app.on_event(key(KeyCode::Up));
        app.on_event(ctrl_p());
        type_text(&mut app, "open the message's thread");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(app.composer.text(), "thread secret draft");
        app.on_event(ctrl_p());
        type_text(&mut app, "close thread");
        app.on_event(key(KeyCode::Enter));
        assert!(!app.thread_open());
        assert_eq!(app.composer.text(), "rust room draft");
        redraw(&mut app, &mut terminal);
    }

    #[test]
    fn palette_blocked_in_viewer_and_auth_or_verify_popups() {
        let (mut app, mut terminal) = drawn();
        // Open design room (which has an image at message index 1) and open the picture viewer.
        let design = app.rooms.iter().position(|r| r.name == "design").unwrap();
        app.open_room(design);
        assert!(app.open_image(1));
        assert!(app.images.viewer_open());
        redraw(&mut app, &mut terminal);

        app.on_event(ctrl_p());
        assert!(app.modal.is_none());
        assert!(app.images.viewer_open());
        app.perform(Action::OpenPalette);
        assert!(app.modal.is_none());
        app.images.close_viewer();

        // Login form blocks Ctrl-P and Action::OpenPalette without typing 'p'.
        app.open_modal(Modal::Login(crate::modal::LoginForm::new(None)));
        app.on_event(ctrl_p());
        assert!(matches!(&app.modal, Some(Modal::Login(f)) if f.homeserver == "matrix.org"));
        app.perform(Action::OpenPalette);
        assert!(matches!(&app.modal, Some(Modal::Login(_))));

        // Recovery form blocks Ctrl-P and Action::OpenPalette without typing 'p'.
        app.open_modal(Modal::Recovery(crate::modal::RecoveryForm::default()));
        app.on_event(ctrl_p());
        assert!(matches!(&app.modal, Some(Modal::Recovery(f)) if f.key.is_empty()));
        app.perform(Action::OpenPalette);
        assert!(matches!(&app.modal, Some(Modal::Recovery(_))));

        // Verification popup blocks Ctrl-P and Action::OpenPalette.
        app.open_modal(Modal::Verify(crate::verify::VerifyPopup::flow()));
        app.on_event(ctrl_p());
        assert!(matches!(&app.modal, Some(Modal::Verify(_))));
        app.perform(Action::OpenPalette);
        assert!(matches!(&app.modal, Some(Modal::Verify(_))));
    }

    #[test]
    fn wide_chars_paste_ctrl_u_and_wheel_scroll() {
        let (mut app, mut terminal) = drawn();
        app.rooms.push(crate::model::Room::new(
            "!wide:x",
            "日本語コミュニティ 🎉 very-long-room-name-for-truncation-testing",
            RoomKind::Room,
        ));
        app.on_event(ctrl_p());
        redraw(&mut app, &mut terminal);

        // Paste wide query longer than popup width and verify redraw + Ctrl-U reset work.
        let long_query = "日本語".repeat(30);
        app.on_event(Event::Paste(long_query));
        redraw(&mut app, &mut terminal);
        app.on_event(key_mod(KeyCode::Char('u'), KeyModifiers::CONTROL));
        redraw(&mut app, &mut terminal);

        // Filter to the wide room and jump to it.
        app.on_event(Event::Paste("日本語".into()));
        redraw(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        assert_eq!(app.rooms[app.active].id, "!wide:x");

        // Mouse wheel scrolls inside the open palette without scrolling the timeline.
        let tl_scroll = app.timeline.scroll;
        app.on_event(ctrl_p());
        redraw(&mut app, &mut terminal);
        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 15,
            modifiers: KeyModifiers::NONE,
        }));
        redraw(&mut app, &mut terminal);
        let scroll = match &app.modal {
            Some(Modal::Palette(p)) => p.scroll,
            _ => unreachable!(),
        };
        assert!(scroll > 0);
        assert_eq!(app.timeline.scroll, tl_scroll);
    }

    #[test]
    fn palette_query_edits_mid_text_and_clicks_to_place() {
        let (mut app, mut terminal) = drawn();
        let ctrl = |c: char| key_mod(KeyCode::Char(c), KeyModifiers::CONTROL);
        let ctrl_key = |code: KeyCode| key_mod(code, KeyModifiers::CONTROL);
        let alt = |c: char| key_mod(KeyCode::Char(c), KeyModifiers::ALT);

        app.on_event(ctrl_p());
        type_text(&mut app, "cafe\u{0301} beta");
        app.on_event(ctrl_key(KeyCode::Left)); // before "beta"
        app.on_event(ctrl('k')); // "cafe\u{0301} "
        app.on_event(key(KeyCode::Backspace)); // "cafe\u{0301}"
        app.on_event(key(KeyCode::Home));
        app.on_event(alt('f'));
        app.on_event(key(KeyCode::Backspace)); // deletes whole e\u{0301}
        app.on_event(key(KeyCode::Char('e')));
        match &app.modal {
            Some(Modal::Palette(p)) => assert_eq!(p.query, "cafe"),
            other => panic!("unexpected modal: {other:?}"),
        }

        // Paste multiline text and click inside the rendered query row.
        app.on_event(ctrl('u'));
        app.on_event(Event::Paste("theme\nsecond".into()));
        redraw(&mut app, &mut terminal);
        let (col, row) = find(&terminal, "theme");
        app.on_event(click(col + 2, row)); // th|eme
        app.on_event(key(KeyCode::Delete)); // thme
        match &app.modal {
            Some(Modal::Palette(p)) => {
                assert_eq!(p.query, "thme");
                assert!(!p.filtered.is_empty());
            }
            other => panic!("unexpected modal: {other:?}"),
        }
    }
}
