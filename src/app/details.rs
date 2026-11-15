//! The message details popup on `App`: opening it for the selected message, asking the
//! worker for the message's source, and the keys and clicks inside it.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::details::{DetailsPopup, DetailsView, SourceState};
use crate::matrix::{EventSource, MatrixCmd};
use crate::modal::Modal;
use crate::model::ItemRef;

impl App {
    /// Opens the details of the selected message. What the timeline knows is shown at
    /// once; the source (and when the message was last edited) follows from the worker.
    pub(super) fn open_message_details(&mut self) {
        let Some(index) = self.timeline.selected else {
            self.show_toast("Select a message first");
            return;
        };
        if !self
            .messages()
            .get(index)
            .is_some_and(|m| !m.is_system_or_marker())
        {
            return;
        }
        let request = self.next_request();
        let room_encrypted = self.room().is_some_and(|room| room.encrypted);
        let message = &self.messages()[index];
        let item = message.item.clone();
        let mut popup = DetailsPopup::new(message, room_encrypted, self.read_markers, request);
        let ask = match (&item, self.matrix.is_some()) {
            (Some(ItemRef::Event(_)), true) => true,
            (_, false) => {
                popup.source = SourceState::Missing("The demo has no source to show".into());
                false
            }
            _ => {
                popup.source = SourceState::Missing(
                    "This message has not reached the server yet, so it has no source".into(),
                );
                false
            }
        };
        self.open_modal(Modal::Details(Box::new(popup)));
        if ask && let (Some(room_id), Some(item)) = (self.room_id(), item) {
            self.send_matrix(MatrixCmd::FetchSource {
                request,
                room_id,
                item,
            });
        }
    }

    pub(super) fn on_event_source(
        &mut self,
        request: u64,
        result: Result<Box<EventSource>, String>,
    ) {
        if let Some(Modal::Details(popup)) = &mut self.modal
            && popup.request == request
        {
            popup.set_source(result);
        }
    }

    /// Esc: from the source back to the facts, from there out.
    pub(super) fn escape_details(&mut self) {
        match &mut self.modal {
            Some(Modal::Details(popup)) if popup.view == DetailsView::Source => popup.flip(),
            Some(Modal::Details(_)) => self.modal = None,
            _ => {}
        }
    }

    pub(super) fn on_details_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Some(Modal::Details(popup)) = &mut self.modal else {
            return;
        };
        let page = popup.page.saturating_sub(1).max(1) as isize;
        match code {
            KeyCode::Esc => self.escape_details(),
            KeyCode::Up => popup.scroll_by(-1),
            KeyCode::Char('k') if plain => popup.scroll_by(-1),
            KeyCode::Down => popup.scroll_by(1),
            KeyCode::Char('j') if plain => popup.scroll_by(1),
            KeyCode::PageUp => popup.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => popup.scroll_by(page),
            KeyCode::Home => popup.scroll = 0,
            // Brought back to the last page when drawn.
            KeyCode::End => popup.scroll = usize::MAX,
            KeyCode::Tab | KeyCode::BackTab => popup.flip(),
            KeyCode::Char('v') if plain => popup.flip(),
            KeyCode::Enter if popup.view == DetailsView::Facts => popup.flip(),
            KeyCode::Char('c') if plain => self.copy_details(),
            _ => {}
        }
    }

    pub(super) fn scroll_details(&mut self, lines: isize) {
        if let Some(Modal::Details(popup)) = &mut self.modal {
            popup.scroll_by(lines);
        }
    }

    /// The "View source" / "Back to details" button.
    pub(super) fn flip_details(&mut self) {
        if let Some(Modal::Details(popup)) = &mut self.modal {
            popup.flip();
        }
    }

    /// Copies the event ID from the facts, the JSON from the source.
    pub(super) fn copy_details(&mut self) {
        let Some(Modal::Details(popup)) = &self.modal else {
            return;
        };
        let copied = match popup.view {
            DetailsView::Facts => popup
                .event_id
                .clone()
                .map(|id| (id, "Event ID copied".to_owned()))
                .ok_or_else(|| "This message has no event ID yet".to_owned()),
            DetailsView::Source => popup.source_json().map(|json| {
                let lines = json.lines().count().max(1);
                (json, format!("Copied {lines} lines of JSON"))
            }),
        };
        match copied {
            Ok((text, toast)) => {
                self.pending_copy = Some(text);
                self.show_toast(toast);
            }
            Err(reason) => self.show_toast(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::app::ui;
    use crate::config::Config;
    use crate::matrix::{MatrixEvent, MatrixHandle, RoomCategory, RoomSummary, SourceKind};
    use crate::model::{Message, MsgKind, SendState};
    use crossterm::event::{Event, KeyEvent};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    const ROOM: &str = "!rats:example.org";

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn summary(id: &str, name: &str) -> RoomSummary {
        RoomSummary {
            encrypted: true,
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

    fn message(id: &str, own: bool, item: ItemRef) -> Message {
        Message {
            id: id.into(),
            item: Some(item),
            sender: if own { "me" } else { "bob" }.into(),
            sender_id: Some(
                if own {
                    "@me:example.org"
                } else {
                    "@bob:example.org"
                }
                .into(),
            ),
            body: format!("message {id}"),
            day: "Today".into(),
            time: "12:00".into(),
            kind: MsgKind::Text,
            encrypted: true,
            own,
            ..Message::default()
        }
    }

    /// An app with an account and a room of two messages: bob's, sent, and ours, on
    /// its way.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, mut commands) = MatrixHandle::for_test();
        let mut app = App::new(&Config::default(), std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@me:example.org".into(),
        });
        app.on_matrix(MatrixEvent::Rooms(vec![summary(ROOM, "Rats")]));
        let mut sending = message("e2", true, ItemRef::Transaction("t2".into()));
        sending.state = SendState::Sending;
        app.on_matrix(MatrixEvent::Timeline {
            room_id: ROOM.into(),
            generation: app.generation,
            messages: vec![
                message("e1", false, ItemRef::Event("$e1:example.org".into())),
                sending,
            ],
            reached_start: false,
            detached: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        while commands.try_recv().is_ok() {}
        (app, terminal, commands)
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn popup(app: &App) -> &DetailsPopup {
        match &app.modal {
            Some(Modal::Details(popup)) => popup,
            other => panic!("the details are not open: {other:?}"),
        }
    }

    fn answer(app: &mut App, request: u64, json: &str) {
        app.on_matrix(MatrixEvent::Source {
            request,
            result: Ok(Box::new(EventSource {
                kind: SourceKind::Decrypted,
                json: json.into(),
                edit_json: None,
                edited_at: None,
                cut: false,
            })),
        });
    }

    #[test]
    fn the_key_opens_the_details_and_asks_for_the_source() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::MessageDetails);
        assert!(app.modal.is_none(), "nothing is selected");
        app.perform(Action::Focus(crate::action::Pane::Timeline));
        app.timeline.selected = Some(0);
        app.on_event(key(KeyCode::Char('i')));
        let request = popup(&app).request;
        let asked: Vec<String> = std::iter::from_fn(|| commands.try_recv().ok())
            .map(|command| command.describe())
            .collect();
        assert_eq!(asked, [format!("FetchSource(request {request})")]);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Message details"), "{text}");
        assert!(text.contains("$e1:example.org"), "{text}");
        assert!(
            text.contains("Encrypted · no doubts about the sender"),
            "{text}"
        );
        assert!(text.contains("Esc close"), "{text}");

        // An answer to another request is not this popup's.
        answer(&mut app, request + 1, "{}");
        assert!(matches!(popup(&app).source, SourceState::Loading));
        answer(&mut app, request, "{\n  \"type\": \"m.room.message\"\n}");
        app.on_event(key(KeyCode::Char('v')));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Message source"), "{text}");
        assert!(text.contains("\"type\": \"m.room.message\""), "{text}");
        assert!(text.contains("decrypted"), "{text}");

        // `c` copies the JSON there, the event ID on the first page.
        app.on_event(key(KeyCode::Char('c')));
        assert_eq!(
            app.pending_copy.take().as_deref(),
            Some("{\n  \"type\": \"m.room.message\"\n}")
        );
        app.on_event(key(KeyCode::Esc));
        assert_eq!(popup(&app).view, DetailsView::Facts);
        app.on_event(key(KeyCode::Char('c')));
        assert_eq!(app.pending_copy.take().as_deref(), Some("$e1:example.org"));
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn a_message_on_its_way_has_no_source_to_ask_for() {
        let (mut app, mut terminal, mut commands) = connected();
        app.perform(Action::Focus(crate::action::Pane::Timeline));
        app.timeline.selected = Some(1);
        app.perform(Action::MessageDetails);
        assert!(commands.try_recv().is_err());
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("none yet"), "{text}");
        assert!(text.contains("sending"), "{text}");
        app.on_event(key(KeyCode::Enter));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("has not reached the server yet"), "{text}");
        app.on_event(key(KeyCode::Char('c')));
        assert!(app.pending_copy.is_none());
    }

    #[test]
    fn a_long_source_scrolls_and_the_hint_stays_in_view_on_a_small_terminal() {
        let (mut app, mut terminal, _commands) = connected();
        app.perform(Action::Focus(crate::action::Pane::Timeline));
        app.timeline.selected = Some(0);
        app.perform(Action::MessageDetails);
        let request = popup(&app).request;
        let json: Vec<String> = (0..200).map(|n| format!("  \"line {n}\": {n},")).collect();
        answer(&mut app, request, &json.join("\n"));
        app.on_event(key(KeyCode::Char('v')));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("\"line 0\""), "{text}");
        assert!(text.contains("Esc back"), "{text}");
        app.on_event(key(KeyCode::End));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("\"line 199\""), "{text}");
        assert!(!text.contains("\"line 0\""), "{text}");
        assert!(text.contains("Esc back"), "{text}");
        // The wheel scrolls it too, and never past the top.
        for _ in 0..200 {
            app.scroll_details(-3);
        }
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("\"line 0\""), "{text}");
    }

    #[test]
    fn snapshots_on_a_small_terminal() {
        let (mut app, mut terminal, _commands) = connected();
        app.perform(Action::Focus(crate::action::Pane::Timeline));
        app.timeline.selected = Some(0);
        app.perform(Action::MessageDetails);
        insta::assert_snapshot!("message_details", screen(&mut app, &mut terminal));
        let request = popup(&app).request;
        answer(
            &mut app,
            request,
            "{\n  \"content\": {\n    \"body\": \"message e1\",\n    \"msgtype\": \"m.text\"\n  },\n  \"event_id\": \"$e1:example.org\",\n  \"origin_server_ts\": 1700000000000,\n  \"sender\": \"@bob:example.org\",\n  \"type\": \"m.room.message\"\n}",
        );
        app.on_event(key(KeyCode::Char('v')));
        insta::assert_snapshot!("message_source", screen(&mut app, &mut terminal));
    }

    #[test]
    fn the_message_menu_and_the_palette_lead_to_the_details() {
        let (mut app, _terminal, _commands) = connected();
        app.perform(Action::Focus(crate::action::Pane::Timeline));
        app.timeline.selected = Some(0);
        app.perform(Action::ContextMenu);
        let Some(Modal::Menu(menu)) = &app.modal else {
            panic!("no menu");
        };
        let at = menu
            .items
            .iter()
            .position(|item| item.label == "Message details")
            .expect("in the menu");
        assert_eq!(menu.items[at].hint, "i");
        app.modal = None;
        app.perform(Action::OpenPalette);
        let Some(Modal::Palette(palette)) = &app.modal else {
            panic!("no palette");
        };
        assert!(
            palette
                .entries
                .iter()
                .any(|entry| entry.title == "Message details")
        );
    }
}
