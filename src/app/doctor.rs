//! The doctor's popup on `App` ("What the app found here", `Action::Doctor`): the
//! facts from what the running app knows, the keys and clicks inside the popup, and
//! copying the report as text.

use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::config::ImageMode;
use crate::doctor::{Facts, Popup};
use crate::matrix::SecretStoreMode;
use crate::modal::Modal;
use crate::theme::Theme;

/// What the running app learned about the place it runs in, for the doctor. Settings
/// by our own names, counts and switches only: nothing of the user's.
#[derive(Debug, Default)]
pub struct Found {
    /// Whether the terminal took the kitty keyboard protocol, as it answered at start;
    /// `None` where no terminal was set up (the tests).
    pub keyboard_enhanced: Option<bool>,
    /// The terminal has reported that its window went to the front or behind.
    pub focus_reports: bool,
    /// The `images` and `secret_store` settings as read at start.
    pub image_mode: ImageMode,
    pub secret_store: SecretStoreMode,
    /// Key bindings in the config that were not understood (the log names them).
    pub key_problems: usize,
    /// Settings whose value was not understood (`Config::ignored`).
    pub ignored_settings: Vec<String>,
    /// The config file was read again after a change and is not valid TOML now.
    pub config_broken: bool,
}

impl App {
    /// The facts for the report: the environment, and what the app itself knows (the
    /// terminal's answers from the start, the settings in force now, the keymap).
    pub(super) fn doctor_facts(&self) -> Facts {
        Facts {
            size: Some((self.screen.width, self.screen.height)),
            truecolor: self.truecolor,
            no_color: self.no_color,
            keyboard_enhanced: self.found.keyboard_enhanced,
            focus_reports: Some(self.found.focus_reports),
            mouse: self.mouse,
            image_mode: self.found.image_mode,
            graphics: Some(self.images.graphics),
            notifications: self.notify_mode,
            do_not_disturb: self.do_not_disturb,
            secret_store: self.found.secret_store,
            own_theme: Theme::builtin(&self.wanted_theme).is_none(),
            key_preset: self.keymap.preset_summary(),
            key_problems: self.found.key_problems,
            unbound: crate::doctor::unbound(&self.keymap),
            ignored_settings: self.found.ignored_settings.clone(),
            config_broken: self.found.config_broken,
            audio: self.audio_found(),
            ..Facts::from_environment()
        }
    }

    /// Opens the report as things are now. Not over another popup (the login form
    /// among them): `matrix-rat doctor` on the command line is there for that.
    pub(super) fn open_doctor(&mut self) {
        if self.modal.is_some() {
            return;
        }
        let facts = self.doctor_facts();
        self.open_doctor_with(&facts);
    }

    pub(super) fn open_doctor_with(&mut self, facts: &Facts) {
        self.open_modal(Modal::Doctor(Popup::new(facts)));
    }

    pub(super) fn on_doctor_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let plain = !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        let Some(Modal::Doctor(popup)) = &mut self.modal else {
            return;
        };
        let page = popup.page.saturating_sub(1).max(1) as isize;
        match code {
            KeyCode::Esc => self.close_modal(),
            KeyCode::Up => popup.scroll_by(-1),
            KeyCode::Char('k') if plain => popup.scroll_by(-1),
            KeyCode::Down => popup.scroll_by(1),
            KeyCode::Char('j') if plain => popup.scroll_by(1),
            KeyCode::PageUp => popup.scroll_by(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => popup.scroll_by(page),
            KeyCode::Home => popup.scroll = 0,
            // Brought back to the last page when drawn.
            KeyCode::End => popup.scroll = usize::MAX,
            KeyCode::Char('c') if plain => self.copy_doctor(),
            _ => {}
        }
    }

    pub(super) fn scroll_doctor(&mut self, lines: isize) {
        if let Some(Modal::Doctor(popup)) = &mut self.modal {
            popup.scroll_by(lines);
        }
    }

    /// The report as plain text, to the clipboard the way every copy goes (through the
    /// terminal), for a bug report.
    pub(super) fn copy_doctor(&mut self) {
        let Some(Modal::Doctor(popup)) = &self.modal else {
            return;
        };
        let text = popup.report.as_text();
        let lines = text.lines().count();
        self.pending_copy = Some(text);
        self.show_toast(format!("Copied the report ({lines} lines)"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, Pane};
    use crate::app::ui;
    use crate::config::Config;
    use crate::matrix::MatrixHandle;
    use crossterm::event::{Event, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    /// Where `needle` is on the screen of the last frame.
    fn find(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let screen = format!("{}", terminal.backend());
        for (row, line) in screen.lines().enumerate() {
            if let Some(byte) = line.find(needle) {
                // The line starts with a quote from the Display impl.
                let col = crate::text::width(&line[..byte]) - 1;
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{screen}");
    }

    /// The demo drawn at this size, with the doctor open on fixed facts: nothing of
    /// the machine the tests run on gets into a snapshot.
    fn opened(width: u16, height: u16) -> (App, Terminal<TestBackend>) {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui::draw(f, &mut app)).unwrap();
        let mut facts = crate::doctor::sample();
        // A few things to say something about.
        facts.truecolor = false;
        facts.xclip = false;
        facts.wl_paste = false;
        facts.unbound = vec!["Reply to selected message"];
        facts.ignored_settings = vec!["images".to_owned()];
        app.open_doctor_with(&facts);
        (app, terminal)
    }

    fn popup(app: &App) -> &Popup {
        match &app.modal {
            Some(Modal::Doctor(popup)) => popup,
            other => panic!("the doctor is not open: {other:?}"),
        }
    }

    #[test]
    fn snapshots_at_100x30_and_80x24() {
        let (mut app, mut terminal) = opened(100, 30);
        insta::assert_snapshot!("doctor_100x30", screen(&mut app, &mut terminal));
        let (mut app, mut terminal) = opened(80, 24);
        insta::assert_snapshot!("doctor_80x24", screen(&mut app, &mut terminal));
    }

    #[test]
    fn the_report_scrolls_and_the_hint_and_the_button_stay() {
        let (mut app, mut terminal) = opened(80, 24);
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("matrix-rat"), "{text}");
        assert!(!text.contains("Settings in the config"), "{text}");
        assert!(text.contains("Esc close"), "{text}");
        assert!(text.contains("Copy as text"), "{text}");
        // To the end: the last line is in view, the first is not, nothing else moved.
        app.on_event(key(KeyCode::End));
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Settings in the config"), "{text}");
        assert!(!text.contains("TERM_PROGRAM"), "{text}");
        assert!(text.contains("Esc close"), "{text}");
        assert!(text.contains("Copy as text"), "{text}");
        let last = popup(&app).scroll;
        assert!(last > 0);
        // Not past the end; a page up, a line up, the wheel to the top.
        app.on_event(key(KeyCode::Down));
        screen(&mut app, &mut terminal);
        assert_eq!(popup(&app).scroll, last);
        app.on_event(key(KeyCode::PageUp));
        assert!(popup(&app).scroll < last);
        app.on_event(key(KeyCode::Up));
        let (column, row) = find(&terminal, "Copy as text");
        for _ in 0..20 {
            app.on_event(mouse(MouseEventKind::ScrollUp, column, row));
        }
        assert_eq!(popup(&app).scroll, 0);
        app.on_event(mouse(MouseEventKind::ScrollDown, column, row));
        assert_eq!(popup(&app).scroll, 2);
        // Esc closes it.
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none());
    }

    #[test]
    fn c_and_the_button_copy_the_report_as_text() {
        let (mut app, mut terminal) = opened(100, 30);
        let text = popup(&app).report.as_text();
        assert!(text.contains("! Settings in the config: not understood: images"));
        app.on_event(key(KeyCode::Char('c')));
        assert_eq!(app.pending_copy.take().as_deref(), Some(text.as_str()));
        let toast = app.toast.as_ref().map(|(toast, _)| toast.clone());
        assert!(
            toast
                .as_deref()
                .is_some_and(|t| t.starts_with("Copied the report")),
            "{toast:?}"
        );
        // Still open: there may be more to read.
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
        screen(&mut app, &mut terminal);
        let (column, row) = find(&terminal, "Copy as text");
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), column, row));
        assert_eq!(app.pending_copy.take().as_deref(), Some(text.as_str()));
        // A click beside the popup closes it.
        app.on_event(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        assert!(app.modal.is_none());
        // A paste does nothing to it.
        app.open_doctor_with(&crate::doctor::sample());
        app.on_event(Event::Paste("pasted".into()));
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
        assert!(app.composer.is_empty());
    }

    #[test]
    fn it_opens_with_no_chat_open_with_an_account_and_from_the_settings() {
        // No chat open: it is not about a chat.
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        screen(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Esc));
        assert!(app.chat_closed());
        assert!(!Action::Doctor.needs_open_chat());
        app.perform(Action::Doctor);
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
        assert!(screen(&mut app, &mut terminal).contains("What the app found"));
        assert!(app.chat_closed());
        assert_eq!(app.focus, Pane::Sidebar);
        // Not over another popup.
        app.modal = None;
        app.perform(Action::OpenSettings);
        app.perform(Action::Doctor);
        assert!(matches!(app.modal, Some(Modal::Settings(_))));
        // The settings' row opens it in their place.
        app.perform(Action::Setting(crate::modal::Setting::Doctor));
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
        app.modal = None;
        app.perform(Action::OpenSettings);
        if let Some(Modal::Settings(menu)) = &mut app.modal {
            *menu = crate::modal::SettingsMenu::select(crate::modal::Setting::Doctor, false);
        }
        app.on_event(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));

        // With an account: nothing is asked of the server.
        let (handle, mut commands) = MatrixHandle::for_test();
        let mut app = App::new(&Config::default(), std::path::PathBuf::new(), Some(handle));
        while commands.try_recv().is_ok() {}
        app.perform(Action::Doctor);
        assert!(matches!(app.modal, Some(Modal::Doctor(_))));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn the_palette_offers_it_without_a_key() {
        let mut app = App::demo();
        app.perform(Action::OpenPalette);
        let Some(Modal::Palette(palette)) = &app.modal else {
            panic!("no palette");
        };
        let entry = palette
            .entries
            .iter()
            .find(|entry| entry.title == "What the app found here")
            .expect("in the palette");
        assert!(entry.enabled);
        assert_eq!(entry.hint, None);
        assert!(
            crate::keymap::Keymap::default()
                .label_for(Action::Doctor)
                .is_none()
        );
    }

    #[test]
    fn the_facts_come_from_what_the_app_knows() {
        let mut app = App::demo();
        app.mouse = false;
        app.do_not_disturb = true;
        app.notify_mode = crate::notify::NotifyMode::Off;
        app.found.keyboard_enhanced = Some(false);
        app.found.key_problems = 2;
        app.found.ignored_settings = vec!["mouse".to_owned()];
        app.found.secret_store = SecretStoreMode::Keyring;
        app.found.image_mode = ImageMode::Off;
        app.on_event(Event::FocusLost);
        let config = Config::parse("[keys.timeline]\nreply = \"none\"\n").unwrap();
        app.keymap = crate::keymap::Keymap::from_config(&config).0;
        let facts = app.doctor_facts();
        assert!(!facts.mouse);
        assert!(facts.do_not_disturb);
        assert_eq!(facts.notifications, crate::notify::NotifyMode::Off);
        assert_eq!(facts.keyboard_enhanced, Some(false));
        assert_eq!(facts.focus_reports, Some(true));
        assert_eq!(facts.key_problems, 2);
        assert_eq!(facts.ignored_settings, ["mouse"]);
        assert_eq!(facts.secret_store, SecretStoreMode::Keyring);
        assert_eq!(facts.image_mode, ImageMode::Off);
        assert_eq!(facts.graphics, Some(app.images.graphics));
        assert_eq!(facts.unbound, ["Reply to selected message"]);
        assert_eq!(facts.key_preset, "default (edited)");
        assert!(!facts.own_theme);
        // Before any focus report, and in a demo that never asked the terminal.
        let app = App::demo();
        let facts = app.doctor_facts();
        assert_eq!(facts.focus_reports, Some(false));
        assert_eq!(facts.keyboard_enhanced, None);
    }

    #[test]
    fn the_config_is_followed_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("matrix-rat-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "mouse = 3\n[keys]\nno_such_action = \"f9\"\n").unwrap();
        let config = Config::load(&file).unwrap();
        let mut app = App::new(&config, dir.join("themes"), None);
        assert_eq!(app.found.ignored_settings, ["mouse"]);
        assert_eq!(app.found.key_problems, 1);
        app.config_file = Some(file.clone());
        // Mended in an editor while the app runs.
        std::fs::write(&file, "images = \"sixel\"\n").unwrap();
        app.reload_theme();
        assert_eq!(app.found.ignored_settings, ["images"]);
        assert_eq!(app.found.key_problems, 0);
        assert!(!app.found.config_broken);
        // Broken: said so, and what was known stays.
        std::fs::write(&file, "images = = 1\n").unwrap();
        app.reload_theme();
        assert!(app.found.config_broken);
        assert_eq!(app.found.ignored_settings, ["images"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
