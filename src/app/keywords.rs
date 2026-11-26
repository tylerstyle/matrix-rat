//! The app's side of the notification keywords popup: opening it, typing and removing,
//! and the worker's answers. The demo has no server and works on a list of its own.

use super::App;
use crate::input::{KeyCode, KeyModifiers};
use crate::keywords::{KeywordsPopup, Pending, check_keyword};
use crate::lineedit::EditKeys;
use crate::matrix::{KeywordsCmd, KeywordsEvent, MatrixCmd, shown_keywords};
use crate::modal::Modal;

impl App {
    /// Opens the popup. With an account, the worker is asked to send the list (under
    /// a new number) now and whenever it changes; the demo shows its own.
    pub(super) fn open_keywords(&mut self) {
        if self.matrix.is_none() {
            let list = shown_keywords(self.keywords.demo.clone());
            self.open_modal(Modal::Keywords(KeywordsPopup::new(Some(list))));
            return;
        }
        if self.logged_out || self.user_id.is_none() {
            self.show_toast("Log in first");
            return;
        }
        self.keywords.next_watch += 1;
        let watch = self.keywords.next_watch;
        self.keywords.watch = Some(watch);
        self.open_modal(Modal::Keywords(KeywordsPopup::new(None)));
        self.send_matrix(MatrixCmd::Keywords(KeywordsCmd::Watch(Some(watch))));
    }

    /// Called a few times a second: tells the worker to stop watching once the popup
    /// is closed (popups close in many places).
    pub(super) fn keywords_watch(&mut self) {
        let open = matches!(self.modal, Some(Modal::Keywords(_)));
        if !open && self.keywords.watch.take().is_some() {
            self.send_matrix(MatrixCmd::Keywords(KeywordsCmd::Watch(None)));
        }
    }

    pub(super) fn on_keywords_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Keywords(popup)) = &mut self.modal else {
            return;
        };
        let page = popup.page.max(1) as isize;
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match (code, mods) {
            (KeyCode::Esc, _) => self.keywords_escape(),
            (KeyCode::Enter, _) => self.add_keyword(),
            (KeyCode::Delete, _) if plain => self.remove_keyword(None),
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => popup.step(-1),
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => popup.step(1),
            (KeyCode::PageUp, _) => popup.step(-page),
            (KeyCode::PageDown, _) => popup.step(page),
            // Everything else types; Del belongs to the list.
            _ => {
                let keys = EditKeys {
                    delete: false,
                    ..EditKeys::FULL
                };
                if popup.input.handle_key(code, mods, keys).changed() {
                    popup.error = None;
                    popup.confirm = None;
                }
            }
        }
    }

    pub(super) fn on_keywords_paste(&mut self, first_line: &str) {
        if let Some(Modal::Keywords(popup)) = &mut self.modal
            && popup.input.paste(first_line).changed()
        {
            popup.error = None;
            popup.confirm = None;
        }
    }

    pub(super) fn click_keywords_input(&mut self, column: usize) {
        if let Some(Modal::Keywords(popup)) = &mut self.modal {
            popup.input.place(column, false);
        }
    }

    pub(super) fn click_keyword_row(&mut self, index: usize) {
        if let Some(Modal::Keywords(popup)) = &mut self.modal {
            popup.select(index);
        }
    }

    pub(super) fn scroll_keywords(&mut self, delta: isize) {
        if let Some(Modal::Keywords(popup)) = &mut self.modal {
            popup.scroll_by(delta.signum());
        }
    }

    /// Esc, or a click beside the popup: steps back from the question, else closes.
    pub(super) fn keywords_escape(&mut self) {
        if let Some(Modal::Keywords(popup)) = &mut self.modal
            && popup.confirm.take().is_none()
        {
            self.modal = None;
        }
    }

    /// Enter: adds what was typed, once it has passed the checks. Not while the list
    /// is loading (it could hold the word already) or a request is under way.
    pub(super) fn add_keyword(&mut self) {
        let demo = self.matrix.is_none();
        let request = self.keywords.next_request + 1;
        let Some(Modal::Keywords(popup)) = &mut self.modal else {
            return;
        };
        if popup.pending.is_some() {
            return;
        }
        popup.confirm = None;
        // "Loading…" says why nothing happens.
        let Some(list) = &popup.list else {
            return;
        };
        let keyword = match check_keyword(popup.input.as_str(), list, popup.total) {
            Ok(keyword) => keyword,
            Err(error) => {
                popup.error = Some(error);
                return;
            }
        };
        popup.error = None;
        if demo {
            popup.input.clear();
            popup.added(&keyword);
            self.keywords.demo.push(keyword);
            return;
        }
        popup.pending = Some((request, Pending::Add(keyword.clone())));
        self.keywords.next_request = request;
        self.send_matrix(MatrixCmd::Keywords(KeywordsCmd::Add { request, keyword }));
    }

    /// Del on the selected keyword, or a click on the remove mark of the one at
    /// `index`: the first time asks, the second time removes.
    pub(super) fn remove_keyword(&mut self, index: Option<usize>) {
        let demo = self.matrix.is_none();
        let request = self.keywords.next_request + 1;
        let Some(Modal::Keywords(popup)) = &mut self.modal else {
            return;
        };
        if popup.pending.is_some() {
            return;
        }
        if let Some(index) = index {
            popup.select(index);
        }
        let Some(keyword) = popup.selected_keyword().map(str::to_owned) else {
            return;
        };
        popup.error = None;
        if popup.confirm.as_deref() != Some(keyword.as_str()) {
            popup.confirm = Some(keyword);
            return;
        }
        popup.confirm = None;
        if demo {
            popup.removed(&keyword);
            self.keywords.demo.retain(|known| *known != keyword);
            return;
        }
        popup.pending = Some((request, Pending::Remove(keyword.clone())));
        self.keywords.next_request = request;
        self.send_matrix(MatrixCmd::Keywords(KeywordsCmd::Remove {
            request,
            keyword,
        }));
    }

    pub(super) fn on_keywords_event(&mut self, event: KeywordsEvent) {
        // News of a session that has ended.
        if self.user_id.is_none() || self.logged_out {
            return;
        }
        match event {
            // For a popup that was closed (and maybe opened again) since.
            KeywordsEvent::List { watch, .. } if self.keywords.watch != Some(watch) => {}
            KeywordsEvent::List {
                keywords, total, ..
            } => {
                if let Some(Modal::Keywords(popup)) = &mut self.modal {
                    popup.set_list(keywords, total);
                }
            }
            KeywordsEvent::Done { request, result } => self.on_keyword_done(request, result),
        }
    }

    /// The outcome of an addition or removal. Shown in the popup that asked; if that
    /// is gone, a failure is told in a toast (a success shows in the list next time).
    fn on_keyword_done(&mut self, request: u64, result: Result<(), String>) {
        let waiting = match &mut self.modal {
            Some(Modal::Keywords(popup))
                if popup
                    .pending
                    .as_ref()
                    .is_some_and(|(asked, _)| *asked == request) =>
            {
                popup.pending.take().map(|(_, what)| (popup, what))
            }
            _ => None,
        };
        match (waiting, result) {
            (Some((popup, Pending::Add(keyword))), Ok(())) => {
                // What was typed meanwhile stays.
                if popup.input.trim() == keyword {
                    popup.input.clear();
                }
                popup.added(&keyword);
            }
            (Some((popup, Pending::Remove(keyword))), Ok(())) => popup.removed(&keyword),
            (Some((popup, _)), Err(error)) => popup.error = Some(error),
            (None, Err(error)) => self.show_toast(error),
            (None, Ok(())) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::action::Action;
    use crate::app::App;
    use crate::config::Config;
    use crate::input::{
        InputEvent, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use crate::keywords::KeywordsPopup;
    use crate::matrix::{KeywordsCmd, KeywordsEvent, MatrixCmd, MatrixEvent, MatrixHandle};
    use crate::modal::{Modal, Setting};
    use crate::ui;

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(col: u16, row: u16) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
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
                // The backend prints a quote before each row.
                let col = crate::text::width(&line[..byte]).saturating_sub(1);
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{text}");
    }

    fn toast(app: &App) -> &str {
        app.toast
            .as_ref()
            .map(|(text, _)| text.as_str())
            .unwrap_or("")
    }

    /// Logged in through a fake worker, on an 80x24 screen.
    fn connected() -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (handle, commands) = MatrixHandle::for_test();
        let config = Config::default();
        let mut app = App::new(&config, std::path::PathBuf::new(), Some(handle));
        app.on_matrix(MatrixEvent::LoggedIn {
            user_id: "@alice:example.org".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        screen(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<KeywordsCmd> {
        let mut out = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::Keywords(command) = command {
                out.push(command);
            }
        }
        out
    }

    fn popup(app: &App) -> &KeywordsPopup {
        match &app.modal {
            Some(Modal::Keywords(popup)) => popup,
            other => panic!("no keywords popup: {other:?}"),
        }
    }

    fn listed(app: &App, keywords: &[&str]) -> MatrixEvent {
        MatrixEvent::Keywords(KeywordsEvent::List {
            watch: app.keywords.watch.expect("a watch"),
            keywords: keywords.iter().map(|k| (*k).to_owned()).collect(),
            total: keywords.len(),
        })
    }

    fn done(request: u64, result: Result<(), &str>) -> MatrixEvent {
        MatrixEvent::Keywords(KeywordsEvent::Done {
            request,
            result: result.map_err(str::to_owned),
        })
    }

    /// The popup open with the list in, nothing sent left over.
    fn with_list(keywords: &[&str]) -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (mut app, terminal, mut commands) = connected();
        app.perform(Action::Setting(Setting::Keywords));
        app.on_matrix(listed(&app, keywords));
        sent(&mut commands);
        (app, terminal, commands)
    }

    fn keywords(app: &App) -> Vec<&str> {
        popup(app).keywords().iter().map(String::as_str).collect()
    }

    #[test]
    fn keywords_open_from_the_settings_load_and_stop_watching_when_closed() {
        let (mut app, mut terminal, mut commands) = connected();
        app.on_event(key(KeyCode::F(2)));
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "Notification keywords");
        app.on_event(click(col, row));
        let [KeywordsCmd::Watch(Some(watch))] = sent(&mut commands)[..] else {
            panic!("no watch asked for");
        };
        assert_eq!(app.keywords.watch, Some(watch));
        let text = screen(&mut app, &mut terminal);
        find(&text, "Loading…");
        find(&text, "Esc close");

        // Nothing can be added before the list is known.
        type_text(&mut app, "deploy");
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());
        assert!(popup(&app).error.is_none());

        app.on_matrix(listed(&app, &["lunch", "release", "sliding sync"]));
        let text = screen(&mut app, &mut terminal);
        insta::assert_snapshot!("keywords_popup", text);
        find(&text, "› lunch");
        find(&text, "3 keywords");

        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
        app.on_tick();
        app.on_tick();
        assert!(matches!(
            sent(&mut commands)[..],
            [KeywordsCmd::Watch(None)]
        ));
        // Opened again (from the palette's action), nothing old is shown.
        app.perform(Action::Setting(Setting::Keywords));
        assert!(popup(&app).list.is_none());
        let [KeywordsCmd::Watch(Some(again))] = sent(&mut commands)[..] else {
            panic!("no watch asked for");
        };
        assert!(again > watch);
    }

    #[test]
    fn a_keyword_is_added_after_the_checks_and_the_list_follows() {
        let (mut app, mut terminal, mut commands) = with_list(&["lunch"]);
        let clear = InputEvent::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let long = "x".repeat(65);
        for (typed, error) in [
            ("", "Type a word to add."),
            ("LUNCH", "That keyword is in the list already."),
            (".hidden", "A keyword cannot start with a dot."),
            ("a/b", "A keyword cannot hold / or \\."),
            (long.as_str(), "A keyword can have at most 64 characters."),
        ] {
            app.on_event(clear.clone());
            app.on_event(InputEvent::Paste(typed.to_owned()));
            app.on_event(key(KeyCode::Enter));
            find(&screen(&mut app, &mut terminal), error);
        }
        assert!(sent(&mut commands).is_empty());
        // Typing clears the complaint.
        app.on_event(clear.clone());
        assert!(popup(&app).error.is_none());

        // Control characters never get into the field; the rest is trimmed.
        app.on_event(InputEvent::Paste("  dep\u{1b}[2Jloy  ".to_owned()));
        app.on_event(key(KeyCode::Enter));
        let asked = sent(&mut commands);
        let [KeywordsCmd::Add { request, keyword }] = &asked[..] else {
            panic!("nothing added");
        };
        assert_eq!(keyword, "dep[2Jloy");
        let request = *request;
        find(&screen(&mut app, &mut terminal), "Adding…");
        // A second Enter while that is under way sends nothing.
        app.on_event(key(KeyCode::Enter));
        assert!(sent(&mut commands).is_empty());

        app.on_matrix(done(request, Ok(())));
        assert_eq!(keywords(&app), ["dep[2Jloy", "lunch"]);
        assert_eq!(popup(&app).selected_keyword(), Some("dep[2Jloy"));
        assert!(popup(&app).input.is_empty());
        // The worker's list follows; it agrees.
        app.on_matrix(listed(&app, &["dep[2Jloy", "lunch"]));
        let text = screen(&mut app, &mut terminal);
        find(&text, "› dep[2Jloy");
        find(&text, "2 keywords");
    }

    #[test]
    fn a_failure_is_told_in_the_popup_or_in_a_toast_once_it_is_closed() {
        let (mut app, mut terminal, mut commands) = with_list(&["lunch"]);
        type_text(&mut app, "deploy");
        app.on_event(key(KeyCode::Enter));
        let [KeywordsCmd::Add { request, .. }] = sent(&mut commands)[..] else {
            panic!("nothing added");
        };
        app.on_matrix(done(
            request,
            Err("Could not add the keyword: the server did not take the change; try again in a moment"),
        ));
        let text = screen(&mut app, &mut terminal);
        find(
            &text,
            "Could not add the keyword: the server did not take the",
        );
        assert_eq!(keywords(&app), ["lunch"]);
        // What was typed is still there to try again.
        assert_eq!(popup(&app).input.as_str(), "deploy");
        app.on_event(key(KeyCode::Enter));
        let [KeywordsCmd::Add { request, .. }] = sent(&mut commands)[..] else {
            panic!("not tried again");
        };
        assert!(popup(&app).error.is_none());

        // Closed before the answer: the failure is told all the same.
        app.on_event(key(KeyCode::Esc));
        app.on_matrix(done(request, Err("Could not add the keyword: offline")));
        assert_eq!(toast(&app), "Could not add the keyword: offline");
        // Answers after a logout are for a session that has ended.
        app.toast = None;
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        app.on_matrix(done(request, Err("Could not add the keyword: offline")));
        assert_eq!(toast(&app), "");
    }

    #[test]
    fn removing_asks_first_and_a_click_on_the_mark_does_the_same() {
        let (mut app, mut terminal, mut commands) = with_list(&["alpha", "beta", "gamma"]);
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Delete));
        let text = screen(&mut app, &mut terminal);
        find(
            &text,
            "Del again to remove, Esc to keep · “beta” will no longer",
        );
        assert!(sent(&mut commands).is_empty());
        // Esc steps back from the question, and only from it.
        app.on_event(key(KeyCode::Esc));
        assert!(popup(&app).confirm.is_none());
        assert!(app.modal.is_some());
        // Moving on drops the question too.
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(popup(&app).confirm.as_deref(), Some("gamma"));
        assert!(sent(&mut commands).is_empty());
        app.on_event(key(KeyCode::Delete));
        let asked = sent(&mut commands);
        let [KeywordsCmd::Remove { request, keyword }] = &asked[..] else {
            panic!("nothing removed");
        };
        assert_eq!(keyword, "gamma");
        let request = *request;
        find(&screen(&mut app, &mut terminal), "Removing…");
        // Nothing else is asked while that is under way.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Delete));
        assert!(popup(&app).confirm.is_none());
        app.on_matrix(done(request, Ok(())));
        assert_eq!(keywords(&app), ["alpha", "beta"]);

        // With the mouse: the mark on the selected row asks, the second click removes.
        let text = screen(&mut app, &mut terminal);
        let (col, row) = find(&text, "remove ");
        let (_, alpha_row) = find(&text, "alpha");
        assert_eq!(
            row,
            alpha_row + 1,
            "the mark is on the selected row:\n{text}"
        );
        app.on_event(click(col + 1, row));
        find(&screen(&mut app, &mut terminal), "“beta” will no longer");
        app.on_event(click(col + 1, row));
        let asked = sent(&mut commands);
        let [KeywordsCmd::Remove { keyword, .. }] = &asked[..] else {
            panic!("nothing removed");
        };
        assert_eq!(keyword, "beta");
        // A click on a row selects it.
        let (col, row) = find(&screen(&mut app, &mut terminal), "alpha");
        app.on_event(click(col, row));
        assert_eq!(popup(&app).selected_keyword(), Some("alpha"));
    }

    #[test]
    fn late_answers_are_dropped_and_a_change_elsewhere_refreshes_the_list() {
        let (mut app, mut terminal, mut commands) = with_list(&["lunch"]);
        let first = app.keywords.watch.unwrap();
        type_text(&mut app, "deploy");
        app.on_event(key(KeyCode::Enter));
        let [KeywordsCmd::Add { request, .. }] = sent(&mut commands)[..] else {
            panic!("nothing added");
        };
        // Closed and opened again before the answers came.
        app.on_event(key(KeyCode::Esc));
        app.perform(Action::Setting(Setting::Keywords));
        assert_ne!(app.keywords.watch, Some(first));
        // The old watch's list is not this popup's.
        app.on_matrix(MatrixEvent::Keywords(KeywordsEvent::List {
            watch: first,
            keywords: vec!["stale".into()],
            total: 1,
        }));
        assert!(popup(&app).list.is_none());
        // Nor is the answer to the old popup's request.
        app.on_matrix(done(request, Ok(())));
        assert!(popup(&app).list.is_none());
        assert!(popup(&app).pending.is_none());

        app.on_matrix(listed(&app, &["deploy", "lunch"]));
        app.on_event(key(KeyCode::Down));
        // Another client removes "lunch" and adds two: the list follows, the
        // selection stays where it can.
        app.on_matrix(listed(&app, &["aardvark", "deploy", "zebra"]));
        assert_eq!(keywords(&app), ["aardvark", "deploy", "zebra"]);
        assert_eq!(popup(&app).selected_keyword(), Some("deploy"));
        let text = screen(&mut app, &mut terminal);
        find(&text, "› deploy");
        assert!(!text.contains("lunch"));
        // A list cut at the limit says so.
        let many: Vec<String> = (0..crate::matrix::MAX_KEYWORDS)
            .map(|n| format!("word{n:03}"))
            .collect();
        app.on_matrix(MatrixEvent::Keywords(KeywordsEvent::List {
            watch: app.keywords.watch.unwrap(),
            keywords: many,
            total: 130,
        }));
        find(
            &screen(&mut app, &mut terminal),
            "100 of 130 keywords shown",
        );
        type_text(&mut app, "more");
        app.on_event(key(KeyCode::Enter));
        find(
            &screen(&mut app, &mut terminal),
            "There are 100 keywords already: remove one first.",
        );
        // The list scrolls to the selection.
        app.on_event(key(KeyCode::End));
        app.on_event(key(KeyCode::PageDown));
        app.on_event(key(KeyCode::PageDown));
        let text = screen(&mut app, &mut terminal);
        assert!(!text.contains("word000"), "{text}");
        find(&text, "Esc close");
    }

    #[test]
    fn the_demo_works_on_a_list_of_its_own() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        app.perform(Action::Setting(Setting::Keywords));
        assert_eq!(keywords(&app), ["lunch", "sliding sync"]);
        type_text(&mut app, "Release");
        app.on_event(key(KeyCode::Enter));
        assert_eq!(keywords(&app), ["lunch", "Release", "sliding sync"]);
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Delete));
        app.on_event(key(KeyCode::Delete));
        assert_eq!(keywords(&app), ["Release", "sliding sync"]);
        // It is still so when opened again.
        app.on_event(key(KeyCode::Esc));
        app.on_tick();
        app.perform(Action::Setting(Setting::Keywords));
        assert_eq!(keywords(&app), ["Release", "sliding sync"]);
        find(&screen(&mut app, &mut terminal), "› Release");
        assert!(app.keywords.watch.is_none());
    }
}
