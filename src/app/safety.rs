//! UI state and handlers for ignoring users, reporting content, and finding people.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyModifiers};

use crate::lineedit::EditKeys;
use crate::manage::Stage;
use crate::matrix::{MatrixCmd, UserMatch};
use crate::modal::{IgnoredUsersPopup, Modal, ReportForm};
use crate::model::ItemRef;

pub(super) const USER_SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

impl super::App {
    /// Whether `user_id` is currently in the session's ignored user list.
    pub(crate) fn is_user_ignored(&self, user_id: &str) -> bool {
        self.ignored_users.contains(user_id)
    }

    /// Sends an ignore or unignore request for `user_id`.
    pub(super) fn send_ignore_user(&mut self, user_id: String, ignore: bool) {
        if user_id.trim().is_empty() {
            return;
        }
        if self.logged_out {
            return self.show_toast("Log in first to manage ignored users");
        }
        if self.matrix.is_none() {
            return self.show_toast("The demo has no account to do this with.");
        }
        let cmd = if ignore {
            MatrixCmd::IgnoreUser { user_id }
        } else {
            MatrixCmd::UnignoreUser { user_id }
        };
        self.send_matrix(cmd);
    }

    pub(super) fn open_ignored_users(&mut self) {
        self.open_modal(Modal::IgnoredUsers(IgnoredUsersPopup::new(
            &self.ignored_users,
        )));
    }

    pub(super) fn open_report_modal(&mut self, index: usize) {
        let Some(room_id) = self.room_id() else {
            return;
        };
        let Some(message) = self.messages().get(index) else {
            return;
        };
        let Some(ItemRef::Event(event_id)) = &message.item else {
            return self.show_toast("Only sent messages can be reported");
        };
        if message.own {
            return self.show_toast("You cannot report your own message");
        }
        let sender_id = message.sender_id.clone();
        let sender_name = message.sender.clone();
        let already_ignored = sender_id
            .as_deref()
            .is_some_and(|id| self.is_user_ignored(id));
        self.open_modal(Modal::Report(ReportForm::new(
            room_id,
            event_id.clone(),
            sender_id,
            sender_name,
            already_ignored,
        )));
    }

    pub(super) fn on_ignored_users_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::IgnoredUsers(popup)) = &mut self.modal else {
            return;
        };
        let page = popup.page.max(1) as isize;
        match (code, mods) {
            (KeyCode::Esc, _) => {
                self.modal = None;
            }
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::ALT) => popup.step(-1),
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => popup.step(1),
            (KeyCode::PageUp, _) => popup.step(-page),
            (KeyCode::PageDown, _) => popup.step(page),
            (KeyCode::Enter, _) => self.unignore_selected(),
            (KeyCode::Delete, m) if m.is_empty() => self.unignore_selected(),
            _ => {
                let keys = EditKeys {
                    arrows: true,
                    home_end: true,
                    delete: false,
                    kill_to_end: true,
                };
                if popup.filter.handle_key(code, mods, keys).changed() {
                    popup.refilter();
                    popup.scroll = 0;
                }
            }
        }
    }

    pub(super) fn on_ignored_users_paste(&mut self, first_line: &str) {
        if let Some(Modal::IgnoredUsers(popup)) = &mut self.modal
            && popup.filter.paste(first_line).changed()
        {
            popup.refilter();
            popup.scroll = 0;
        }
    }

    pub(super) fn unignore_selected(&mut self) {
        let user_id = match &self.modal {
            Some(Modal::IgnoredUsers(popup)) => popup.selected_user().map(str::to_owned),
            _ => None,
        };
        if let Some(user_id) = user_id {
            self.send_ignore_user(user_id, false);
        }
    }

    pub(super) fn unignore_at(&mut self, filtered_index: usize) {
        let user_id = match &self.modal {
            Some(Modal::IgnoredUsers(popup)) => popup
                .filtered
                .get(filtered_index)
                .and_then(|&all_idx| popup.all.get(all_idx))
                .cloned(),
            _ => None,
        };
        if let Some(user_id) = user_id {
            self.send_ignore_user(user_id, false);
        }
    }

    pub(super) fn scroll_ignored_users(&mut self, lines: isize) {
        if let Some(Modal::IgnoredUsers(popup)) = &mut self.modal {
            popup.scroll_by(lines);
        }
    }

    pub(super) fn on_report_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Report(form)) = &mut self.modal else {
            return;
        };
        match (code, mods) {
            (KeyCode::Esc, _) => {
                self.modal = None;
            }
            (KeyCode::Tab, m) if m.is_empty() => form.step(1),
            (KeyCode::BackTab, _) => form.step(-1),
            (KeyCode::Down, m) if m.is_empty() => form.step(1),
            (KeyCode::Up, m) if m.is_empty() => form.step(-1),
            (KeyCode::Char(' '), m) if m.is_empty() && form.focus != ReportForm::REASON => {
                if Some(form.focus) == form.toggle_slot() {
                    form.flip_ignore();
                } else if form.focus == form.submit_slot() {
                    self.submit_report();
                }
            }
            (KeyCode::Enter, _) => self.submit_report(),
            _ if form.focus == ReportForm::REASON
                && form.reason.handle_key(code, mods, EditKeys::FULL).changed() =>
            {
                form.error = None;
            }
            _ => {}
        }
    }

    pub(super) fn on_report_paste(&mut self, first_line: &str) {
        if let Some(Modal::Report(form)) = &mut self.modal {
            form.focus = ReportForm::REASON;
            if form.reason.paste(first_line).changed() {
                form.error = None;
            }
        }
    }

    pub(super) fn submit_report(&mut self) {
        let Some(Modal::Report(form)) = &mut self.modal else {
            return;
        };
        if self.logged_out {
            form.error = Some("Log in first to report messages".to_owned());
            return;
        }
        if self.matrix.is_none() {
            form.error = Some("The demo has no account to do this with.".to_owned());
            return;
        }
        let reason_text = form.reason.as_str().trim().to_owned();
        let reason = (!reason_text.is_empty()).then_some(reason_text);
        let cmd = MatrixCmd::ReportContent {
            room_id: form.room_id.clone(),
            event_id: form.event_id.clone(),
            reason,
        };
        let also_ignore = form.also_ignore;
        let sender_id = form.sender_id.clone();
        self.modal = None;
        self.send_matrix(cmd);
        if also_ignore && let Some(user_id) = sender_id {
            self.send_ignore_user(user_id, true);
        } else {
            self.show_toast("Sending report…");
        }
    }

    pub(super) fn on_ignored_users_list(&mut self, users: Vec<String>) {
        self.ignored_users = users.into_iter().collect();
        self.sync_ignored_modal();
        self.prune_ignored_from_user_search();
        self.refresh_unread_line_for_ignored();
    }

    pub(super) fn on_ignore_result(
        &mut self,
        user_id: String,
        ignored: bool,
        result: Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                if ignored {
                    self.ignored_users.insert(user_id);
                    self.show_toast("User ignored");
                } else {
                    self.ignored_users.remove(&user_id);
                    self.show_toast("Removed from ignored list");
                }
                self.sync_ignored_modal();
                self.prune_ignored_from_user_search();
                self.refresh_unread_line_for_ignored();
            }
            Err(err) => {
                let verb = if ignored { "ignore" } else { "unignore" };
                self.show_toast(format!("Could not {verb} user: {err}"));
            }
        }
    }

    pub(super) fn on_report_result(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.show_toast("Message reported"),
            Err(err) => self.show_toast(format!("Could not report message: {err}")),
        }
    }

    fn sync_ignored_modal(&mut self) {
        if let Some(Modal::IgnoredUsers(popup)) = &mut self.modal {
            popup.sync_users(&self.ignored_users);
        }
    }

    fn prune_ignored_from_user_search(&mut self) {
        if let Some(Modal::New(popup)) = &mut self.modal {
            popup
                .user_results
                .retain(|person| !self.ignored_users.contains(&person.user_id));
            if let Some(selected) = popup.user_selected
                && selected >= popup.user_results.len()
            {
                popup.user_selected = popup.user_results.len().checked_sub(1);
            }
        }
    }

    pub(super) fn on_user_search_input_changed(&mut self) {
        let has_matrix = self.matrix.is_some();
        let next_gen = self.user_search_gen.wrapping_add(1);
        self.user_search_gen = next_gen;
        let was_debouncing = self.user_search_debounce.take().is_some();

        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        let had_worker_task = popup.user_searching && !was_debouncing;
        if !matches!(popup.stage, Stage::Direct | Stage::Invite) {
            popup.clear_user_search();
            if has_matrix && had_worker_task {
                self.send_matrix(MatrixCmd::CancelUserSearch);
            }
            return;
        }
        let query = popup.user.as_str().trim().to_owned();
        if query.is_empty() {
            popup.clear_user_search();
            if has_matrix && had_worker_task {
                self.send_matrix(MatrixCmd::CancelUserSearch);
            }
            return;
        }
        popup.user_selected = None;
        popup.user_searching = has_matrix;
        popup.user_search_gen = Some(next_gen);
        if has_matrix {
            if had_worker_task {
                self.send_matrix(MatrixCmd::CancelUserSearch);
            }
            self.user_search_debounce = Some(Instant::now());
        }
    }

    pub(super) fn cancel_user_search(&mut self) {
        self.user_search_gen = self.user_search_gen.wrapping_add(1);
        let was_debouncing = self.user_search_debounce.take().is_some();
        let had_worker_task = match &mut self.modal {
            Some(Modal::New(popup)) => {
                let active = popup.user_searching && !was_debouncing;
                popup.clear_user_search();
                active
            }
            _ => false,
        };
        if had_worker_task && self.matrix.is_some() {
            self.send_matrix(MatrixCmd::CancelUserSearch);
        }
    }

    pub(super) fn user_search_debounce_deadline(&self) -> Option<tokio::time::Instant> {
        self.user_search_debounce
            .map(|at| (at + USER_SEARCH_DEBOUNCE).into())
    }

    pub(super) fn tick_user_search(&mut self) {
        if !self
            .user_search_debounce
            .is_some_and(|at| at.elapsed() >= USER_SEARCH_DEBOUNCE)
        {
            return;
        }
        self.user_search_debounce = None;
        let dispatch = match &self.modal {
            Some(Modal::New(popup))
                if matches!(popup.stage, Stage::Direct | Stage::Invite) && popup.busy.is_none() =>
            {
                let query = popup.user.as_str().trim().to_owned();
                popup
                    .user_search_gen
                    .filter(|_| !query.is_empty())
                    .map(|generation| (generation, query))
            }
            _ => None,
        };
        if let Some((generation, query)) = dispatch
            && self.matrix.is_some()
        {
            self.send_matrix(MatrixCmd::SearchUsers { generation, query });
        }
    }

    pub(super) fn on_user_search_results(&mut self, generation: u64, mut results: Vec<UserMatch>) {
        let own = self.user_id.as_deref();
        let ignored = &self.ignored_users;
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        if !matches!(popup.stage, Stage::Direct | Stage::Invite)
            || popup.busy.is_some()
            || popup.user_search_gen != Some(generation)
        {
            return;
        }
        results.retain(|person| {
            !ignored.contains(&person.user_id) && own != Some(person.user_id.as_str())
        });
        popup.user_searching = false;
        popup.user_results = results;
        popup.user_selected = None;
    }

    pub(super) fn pick_user_search(&mut self, index: usize) {
        let Some(Modal::New(popup)) = &mut self.modal else {
            return;
        };
        if !matches!(popup.stage, Stage::Direct | Stage::Invite) || popup.busy.is_some() {
            return;
        }
        let Some(picked) = popup.user_results.get(index).cloned() else {
            return;
        };
        popup.user.set(&picked.user_id);
        popup.error = None;
        self.cancel_user_search();
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::action::Action;
    use crate::app::App;
    use crate::config::Config;
    use crate::manage::{Click, Confirm, NewPopup, Stage};
    use crate::matrix::{
        ManageCmd, ManageEvent, MatrixCmd, MatrixEvent, MatrixHandle, MemberRole, NotifyLevel,
        RoomCategory, RoomDetails, RoomMember, RoomSummary, UserMatch,
    };
    use crate::modal::{IgnoredUsersPopup, Modal, ReportForm};
    use crate::model::{ItemRef, Message, MsgKind};
    use crate::ui;

    const ROOM: &str = "!room:example.org";
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

    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@alice:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Rooms(vec![RoomSummary {
            id: ROOM.into(),
            name: "Rats".into(),
            category: RoomCategory::Room,
            notifications: 0,
            server_notifications: 0,
            mention: false,
            unread: false,
            marked_unread: false,
            preview: None,
            avatar: None,
            alias: None,
        }]));
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM.into(),
            generation: app.generation,
            messages: vec![Message {
                id: "e1".into(),
                item: Some(ItemRef::Event("$ev1:example.org".into())),
                sender: "bob".into(),
                sender_id: Some(BOB.into()),
                body: "hello from bob".into(),
                day: "Today".into(),
                time: "12:00".into(),
                kind: MsgKind::Text,
                own: false,
                ..Message::default()
            }],
            reached_start: false,
            detached: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        (app, terminal, commands)
    }

    fn drain_cmds(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = commands.try_recv() {
            out.push(cmd);
        }
        out
    }

    fn open_room_info_with_members(app: &mut App, commands: &mut UnboundedReceiver<MatrixCmd>) {
        app.on_event(key(KeyCode::F(4)));
        drain_cmds(commands);
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Info {
            room_id: ROOM.into(),
            result: Ok(Box::new(RoomDetails {
                name: "Rats".into(),
                topic: "Topic".into(),
                alias: Some("#rats:example.org".into()),
                encrypted: Some(true),
                direct: false,
                joined: 3,
                invited: 0,
                favourite: false,
                notify: Some(NotifyLevel::All),
                can_rename: false,
                can_set_topic: false,
                can_invite: true,
                members: vec![
                    RoomMember {
                        user_id: "@alice:example.org".into(),
                        name: "alice".into(),
                        role: MemberRole::Admin,
                        invited: false,
                        own: true,
                        removable: false,
                    },
                    RoomMember {
                        user_id: BOB.into(),
                        name: "bob".into(),
                        role: MemberRole::User,
                        invited: false,
                        own: false,
                        removable: true,
                    },
                    RoomMember {
                        user_id: CAROL.into(),
                        name: "carol".into(),
                        role: MemberRole::Moderator,
                        invited: false,
                        own: false,
                        removable: false,
                    },
                ],
                members_error: None,
                members_loading: false,
                members_partial: false,
            })),
        }));
    }

    #[test]
    fn message_menu_ignore_unignore_confirm_and_toggle() {
        let (mut app, mut terminal, mut commands) = connected();
        drain_cmds(&mut commands);

        app.perform(Action::SelectOlder);
        app.perform(Action::ContextMenu);
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Ignore user");
        // First click asks for confirmation without sending a command.
        app.on_event(click(col, row));
        assert!(drain_cmds(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Again to ignore");
        // Second click confirms and closes menu cleanly.
        app.on_event(click(col, row));
        assert!(app.modal.is_none());
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::IgnoreUser { user_id }] if user_id == BOB
        ));

        // After worker reports user is ignored, menu offers "Stop ignoring" with confirm.
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![BOB.into()]));
        app.perform(Action::ContextMenu);
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Stop ignoring");
        app.on_event(click(col, row));
        assert!(drain_cmds(&mut commands).is_empty());
        let text = screen(&mut app, &mut terminal);
        find(&text, "Again to stop ignoring");
        app.on_event(click(col, row));
        assert!(app.modal.is_none());
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::UnignoreUser { user_id }] if user_id == BOB
        ));
    }

    #[test]
    fn room_info_ignore_unignore_confirm_shortcut_and_click() {
        let (mut app, mut terminal, mut commands) = connected();
        open_room_info_with_members(&mut app, &mut commands);
        type_text(&mut app, "alice");
        // Own account cannot be ignored.
        app.on_event(key_mod(KeyCode::Char('i'), KeyModifiers::ALT));
        let text = screen(&mut app, &mut terminal);
        find(&text, "You cannot ignore your own account.");
        assert!(drain_cmds(&mut commands).is_empty());

        // Filter to bob; first Alt-I arms confirm, Esc clears confirm without closing modal.
        for _ in 0..5 {
            app.on_event(key(KeyCode::Backspace));
        }
        type_text(&mut app, "bob");
        app.on_event(key_mod(KeyCode::Char('i'), KeyModifiers::ALT));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Confirm ignore");
        assert!(drain_cmds(&mut commands).is_empty());

        app.on_event(key(KeyCode::Esc));
        assert!(matches!(app.modal, Some(Modal::RoomInfo(_))));
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("Confirm ignore"));

        // Press Alt-I twice -> sends IgnoreUser.
        app.on_event(key_mod(KeyCode::Char('i'), KeyModifiers::ALT));
        app.on_event(key_mod(KeyCode::Char('i'), KeyModifiers::ALT));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::IgnoreUser { user_id }] if user_id == BOB
        ));

        // Ignored list sync updates member tag + " unignore " pill; double-click unignores.
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![BOB.into()]));
        let text = screen(&mut app, &mut terminal);
        find(&text, "ignored");
        let (col, row) = find(&text, " unignore ");
        app.on_event(click(col + 1, row));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Confirm unignore");
        app.on_event(click(col + 1, row));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::UnignoreUser { user_id }] if user_id == BOB
        ));
    }

    #[test]
    fn settings_ignored_users_filter_unignore_and_live_sync() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![CAROL.into(), BOB.into()]));
        drain_cmds(&mut commands);

        app.on_event(key(KeyCode::F(2)));
        let text = screen(&mut app, &mut terminal);
        find(&text, "2 users");
        let (col, row) = find(&text, "Ignored users");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::IgnoredUsers(_))));

        // Sorted order: bob then carol.
        if let Some(Modal::IgnoredUsers(popup)) = &app.modal {
            assert_eq!(popup.all, vec![BOB.to_owned(), CAROL.to_owned()]);
        } else {
            panic!("expected IgnoredUsers modal");
        }
        // Snapshot the Ignored users modal cleanly before filtering.
        insta::assert_snapshot!("ignored_users_popup", screen(&mut app, &mut terminal));

        // Filter narrows cleanly to carol and unignores on Delete.
        type_text(&mut app, "car");
        let text = screen(&mut app, &mut terminal);
        find(&text, CAROL);
        assert!(!text.contains(BOB));
        app.on_event(key(KeyCode::Delete));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::UnignoreUser { user_id }] if user_id == CAROL
        ));

        // Worker confirms removal via IgnoredUsers list -> live sync updates open modal.
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![BOB.into()]));
        if let Some(Modal::IgnoredUsers(popup)) = &app.modal {
            assert_eq!(popup.all, vec![BOB.to_owned()]);
        }
        // Clear filter and click " unignore " pill on remaining entry.
        for _ in 0..3 {
            app.on_event(key(KeyCode::Backspace));
        }
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, " unignore ");
        app.on_event(click(col + 1, row));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::UnignoreUser { user_id }] if user_id == BOB
        ));
    }

    #[test]
    fn report_form_with_and_without_also_ignore() {
        let (mut app, mut terminal, mut commands) = connected();
        drain_cmds(&mut commands);

        app.perform(Action::SelectOlder);
        app.perform(Action::ContextMenu);
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Report…");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Report(_))));

        type_text(&mut app, "Spam link");
        app.on_event(key(KeyCode::Tab));
        app.on_event(key(KeyCode::Char(' ')));
        insta::assert_snapshot!("report_form_modal", screen(&mut app, &mut terminal));

        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [
                MatrixCmd::ReportContent {
                    room_id,
                    event_id,
                    reason: Some(reason),
                },
                MatrixCmd::IgnoreUser { user_id }
            ] if room_id == ROOM && event_id == "$ev1:example.org" && reason == "Spam link" && user_id == BOB
        ));

        app.on_matrix(MatrixEvent::ReportResult(Ok(())));
        assert_eq!(
            app.toast.as_ref().map(|t| t.0.as_str()),
            Some("Message reported")
        );

        // If user is already ignored, "Also ignore this user" toggle is omitted.
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![BOB.into()]));
        app.open_report_modal(0);
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("Also ignore this user"));
        app.on_event(key(KeyCode::Enter));
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::ReportContent { reason: None, .. }]
        ));
    }

    #[test]
    fn debug_redacts_private_safety_data() {
        let popup = IgnoredUsersPopup::new(&[BOB.to_owned()]);
        let mut form = ReportForm::new(
            ROOM.into(),
            "$secret_event:example.org".into(),
            Some("@secret_user:example.org".into()),
            "Secret Name".into(),
            false,
        );
        form.reason.set("super secret abuse reason");
        let m = UserMatch {
            user_id: "@secret_user:example.org".into(),
            display_name: Some("Secret Name".into()),
            avatar: Some("mxc://example.org/secretavatar".into()),
        };
        let cmds = [
            MatrixCmd::IgnoreUser {
                user_id: BOB.into(),
            },
            MatrixCmd::UnignoreUser {
                user_id: BOB.into(),
            },
            MatrixCmd::ReportContent {
                room_id: ROOM.into(),
                event_id: "$secret_event:example.org".into(),
                reason: Some("secret reason".into()),
            },
            MatrixCmd::SearchUsers {
                generation: 9,
                query: "secret query".into(),
            },
        ];
        let events = [
            MatrixEvent::IgnoredUsers(vec![BOB.into()]),
            MatrixEvent::IgnoreResult {
                user_id: BOB.into(),
                ignored: true,
                result: Ok(()),
            },
            MatrixEvent::UserSearchResults {
                generation: 9,
                results: vec![m.clone()],
            },
        ];
        let confirms = [Confirm::Ignore(BOB.into()), Confirm::Unignore(BOB.into())];
        let combined = format!("{popup:?} {form:?} {m:?} {cmds:?} {events:?} {confirms:?}");
        for secret in [
            BOB,
            "$secret_event",
            "@secret_user",
            "Secret Name",
            "secretavatar",
            "super secret abuse reason",
            "secret reason",
            "secret query",
        ] {
            assert!(!combined.contains(secret), "leaked {secret:?}: {combined}");
        }
    }

    #[test]
    fn empty_timeline_reset_clears_reached_start() {
        let (mut app, _terminal, _commands) = connected();
        app.timeline.reached_start = true;
        app.timeline.empty_batches = 3;
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM.into(),
            generation: app.generation,
            messages: Vec::new(),
            reached_start: false,
            detached: None,
        });
        assert!(!app.timeline.reached_start);
        assert_eq!(app.timeline.empty_batches, 0);
    }

    #[test]
    fn user_search_debounce_generation_cancel_and_selection() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![
            "@ignored:example.org".into(),
        ]));
        drain_cmds(&mut commands);

        app.perform(Action::Manage(Click::Stage(Stage::Direct)));
        type_text(&mut app, "al");
        // Debounce armed, nothing dispatched immediately.
        assert!(app.user_search_debounce.is_some());
        assert!(drain_cmds(&mut commands).is_empty());

        let g1 = app.user_search_gen;
        app.user_search_debounce = Instant::now().checked_sub(Duration::from_millis(300));
        app.tick_user_search();
        assert!(app.user_search_debounce.is_none());
        let sent = drain_cmds(&mut commands);
        assert!(matches!(
            sent.as_slice(),
            [MatrixCmd::SearchUsers { generation, query }]
                if *generation == g1 && query == "al"
        ));

        // Typing another char cancels g1 and arms g2.
        type_text(&mut app, "i");
        let g2 = app.user_search_gen;
        assert!(g2 > g1);
        let sent = drain_cmds(&mut commands);
        assert!(matches!(sent.as_slice(), [MatrixCmd::CancelUserSearch]));
        app.user_search_debounce = Instant::now().checked_sub(Duration::from_millis(300));
        app.tick_user_search();
        assert!(matches!(
            drain_cmds(&mut commands).as_slice(),
            [MatrixCmd::SearchUsers { generation, query }]
                if *generation == g2 && query == "ali"
        ));

        // Late results for stale g1 are discarded cleanly.
        app.on_matrix(MatrixEvent::UserSearchResults {
            generation: g1,
            results: vec![UserMatch {
                user_id: "@stale:example.org".into(),
                display_name: Some("Stale".into()),
                avatar: None,
            }],
        });
        if let Some(Modal::New(popup)) = &app.modal {
            assert!(popup.user_results.is_empty());
            assert!(popup.user_searching);
        }

        // Results for g2 filter out own user and ignored user IDs.
        app.on_matrix(MatrixEvent::UserSearchResults {
            generation: g2,
            results: vec![
                UserMatch {
                    user_id: "@alice:example.org".into(),
                    display_name: Some("Alice".into()),
                    avatar: None,
                },
                UserMatch {
                    user_id: "@ignored:example.org".into(),
                    display_name: Some("Ignored".into()),
                    avatar: None,
                },
                UserMatch {
                    user_id: BOB.into(),
                    display_name: Some("Bob".into()),
                    avatar: None,
                },
                UserMatch {
                    user_id: CAROL.into(),
                    display_name: Some("Carol".into()),
                    avatar: None,
                },
            ],
        });
        app.on_event(key(KeyCode::Down));
        insta::assert_snapshot!("user_search_dropdown", screen(&mut app, &mut terminal));

        // Navigate back up to None, then down twice -> pick Carol with Enter.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(drain_cmds(&mut commands).is_empty());
        if let Some(Modal::New(popup)) = &app.modal {
            assert_eq!(popup.user.as_str(), CAROL);
            assert!(popup.user_results.is_empty());
            assert_eq!(popup.user_selected, None);
        } else {
            panic!("expected New modal");
        }

        // Direct @user:server submits immediately without waiting for lookup timer.
        if let Some(Modal::New(popup)) = &mut app.modal {
            popup.user.set("@eve:example.org");
        }
        app.on_user_search_input_changed();
        drain_cmds(&mut commands);
        assert!(app.user_search_debounce.is_some());
        app.on_event(key(KeyCode::Enter));
        assert!(app.user_search_debounce.is_none());
        let sent = drain_cmds(&mut commands);
        let req1 = sent
            .iter()
            .find_map(|c| match c {
                MatrixCmd::Manage(ManageCmd::DirectChat {
                    request,
                    user_id,
                    create: true,
                }) if user_id == "@eve:example.org" => Some(*request),
                _ => None,
            })
            .expect("expected direct chat command for eve");

        // CancelUserSearch is sent if submitted while worker lookup is in flight.
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Opened {
            request: req1,
            result: Err("Cancelled".into()),
        }));
        if let Some(Modal::New(popup)) = &mut app.modal {
            popup.user.set("@frank:example.org");
        }
        app.on_user_search_input_changed();
        app.user_search_debounce = Instant::now().checked_sub(Duration::from_millis(300));
        app.tick_user_search();
        drain_cmds(&mut commands);
        app.on_event(key(KeyCode::Enter));
        let sent = drain_cmds(&mut commands);
        assert!(
            sent.iter()
                .any(|c| matches!(c, MatrixCmd::CancelUserSearch))
        );
        let req2 = sent
            .iter()
            .find_map(|c| match c {
                MatrixCmd::Manage(ManageCmd::DirectChat {
                    request,
                    user_id,
                    create: true,
                }) if user_id == "@frank:example.org" => Some(*request),
                _ => None,
            })
            .expect("expected direct chat command for frank");

        // Empty results / disabled user directory degrade silently without error banner.
        app.on_matrix(MatrixEvent::Manage(ManageEvent::Opened {
            request: req2,
            result: Err("Cancelled".into()),
        }));
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Manage(Click::Stage(Stage::Direct)));
        type_text(&mut app, "nobody");
        let g = app.user_search_gen;
        app.user_search_debounce = Instant::now().checked_sub(Duration::from_millis(300));
        app.tick_user_search();
        drain_cmds(&mut commands);
        app.on_matrix(MatrixEvent::UserSearchResults {
            generation: g,
            results: Vec::new(),
        });
        if let Some(Modal::New(popup)) = &app.modal {
            assert!(popup.error.is_none());
            assert!(popup.user_results.is_empty());
            assert!(!popup.user_searching);
        }
        // Emptying input clears searching state and notifies worker.
        for _ in 0..6 {
            app.on_event(key(KeyCode::Backspace));
        }
        assert!(app.user_search_debounce.is_none());
    }

    #[test]
    fn reset_session_clears_safety_state() {
        let (mut app, _terminal, _commands) = connected();
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![BOB.into()]));
        app.open_ignored_users();
        app.user_search_debounce = Some(Instant::now());
        let before_gen = app.user_search_gen;
        app.reset_session();
        assert!(app.ignored_users.is_empty());
        assert!(app.user_search_debounce.is_none());
        assert!(app.user_search_gen > before_gen);
        assert!(app.modal.is_none());

        app.modal = Some(Modal::New(NewPopup::new(Stage::Direct)));
        if let Some(Modal::New(popup)) = &mut app.modal {
            popup.user_results.push(UserMatch {
                user_id: BOB.into(),
                display_name: None,
                avatar: None,
            });
            popup.user_searching = true;
        }
        app.reset_session();
        if let Some(Modal::New(popup)) = &app.modal {
            assert!(popup.user_results.is_empty());
            assert!(!popup.user_searching);
        }
    }

    #[test]
    fn suppressed_notice_prevents_count_fallback_notification_while_click_still_opens_room() {
        let (mut app, _terminal, mut commands) = connected();
        app.notify_mode = crate::notify::NotifyMode::Terminal;
        app.window_focused = false;
        app.session_started = Some(Instant::now() - Duration::from_secs(25));

        let base_room = |id: &str, name: &str, count: u64| RoomSummary {
            id: id.into(),
            name: name.into(),
            category: RoomCategory::Room,
            notifications: count,
            server_notifications: count,
            mention: false,
            unread: count > 0,
            marked_unread: false,
            preview: None,
            avatar: None,
            alias: None,
        };
        app.on_matrix(MatrixEvent::Rooms(vec![
            base_room(ROOM, "Rats", 0),
            base_room("!other:example.org", "Other", 0),
        ]));

        // Server bumps notification count for `!other`, queueing a fallback
        // count-based notice, while the worker reports that the detailed notice
        // was suppressed because its sender is ignored.
        app.on_matrix(MatrixEvent::Rooms(vec![
            base_room(ROOM, "Rats", 0),
            base_room("!other:example.org", "Other", 1),
        ]));
        assert_eq!(app.count_notices.len(), 1);
        app.on_matrix(MatrixEvent::SuppressedNotice {
            room_id: "!other:example.org".into(),
        });
        assert!(app.count_notices.is_empty());
        app.release_count_notices(Duration::ZERO);
        assert!(
            app.pending_notices.is_empty(),
            "count fallback must not fire desktop notice for suppressed ignored sender"
        );

        // If a normal non-ignored notification is clicked, NoticeClicked still opens
        // its target room cleanly.
        drain_cmds(&mut commands);
        app.on_matrix(MatrixEvent::NoticeClicked {
            room_id: "!other:example.org".into(),
        });
        assert_eq!(app.active, 1);
        assert_eq!(app.room_id().as_deref(), Some("!other:example.org"));
        assert!(drain_cmds(&mut commands).iter().any(
            |c| matches!(c, MatrixCmd::OpenRoom { room_id, .. } if room_id == "!other:example.org")
        ));
    }

    #[test]
    fn jump_to_ignored_message_shows_clear_toast_and_keeps_current_view() {
        let (mut app, _terminal, mut commands) = connected();
        app.on_matrix(MatrixEvent::IgnoredUsers(vec![BOB.into()]));
        // Add a non-ignored message replying to an ignored message (`$ev_ignored`),
        // mirroring `convert.rs` (`reply_event` still points to the ignored id).
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM.into(),
            generation: app.generation,
            messages: vec![Message {
                id: "reply1".into(),
                item: Some(ItemRef::Event("$ev_reply:example.org".into())),
                sender: "carol".into(),
                sender_id: Some("@carol:example.org".into()),
                body: "see above".into(),
                day: "Today".into(),
                time: "12:01".into(),
                kind: MsgKind::Text,
                reply_to: Some((String::new(), "message from ignored user".into())),
                reply_event: Some("$ev_ignored:example.org".into()),
                ..Message::default()
            }],
            reached_start: true,
            detached: None,
        });
        app.timeline.selected = Some(0);
        drain_cmds(&mut commands);

        app.jump_to_reply(0);
        let cmd = drain_cmds(&mut commands)
            .into_iter()
            .find_map(|c| match c {
                MatrixCmd::OpenEvent {
                    room_id,
                    event_id,
                    generation,
                    ..
                } => Some((room_id, event_id, generation)),
                _ => None,
            })
            .expect("expected OpenEvent command for ignored parent");
        assert_eq!(cmd.0, ROOM);
        assert_eq!(cmd.1, "$ev_ignored:example.org");
        assert!(app.pending_jump.is_some());

        // Worker rejects opening an event from an ignored sender with a clear reason.
        app.on_matrix(MatrixEvent::JumpFailed {
            generation: cmd.2,
            reason: "That message is from an ignored user.".into(),
        });
        assert!(app.pending_jump.is_none());
        assert_eq!(
            app.toast.as_ref().map(|t| t.0.as_str()),
            Some("That message is from an ignored user.")
        );
        // Active room and selection stay intact.
        assert_eq!(app.room_id().as_deref(), Some(ROOM));
        assert_eq!(app.timeline.selected, Some(0));
    }
}
