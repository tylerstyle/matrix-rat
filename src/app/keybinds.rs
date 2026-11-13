//! Key-bindings editor modal interaction and config persistence.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::App;
use crate::action::Action;
use crate::keymap::{
    self, Context, Editor, EditorButton, EditorStage, Keymap, Preset, format_label,
    persist_overrides,
};
use crate::modal::Modal;

impl App {
    pub(super) fn open_keybinds_editor(&mut self) {
        self.open_modal(Modal::Keybinds(Editor::new(&self.keymap)));
    }

    pub(super) fn is_keybinds_capturing(&self) -> bool {
        matches!(
            self.modal,
            Some(Modal::Keybinds(ref editor)) if matches!(editor.stage, EditorStage::Capture)
        )
    }

    pub(super) fn on_keybinds_key(&mut self, raw_key: KeyEvent, code: KeyCode, mods: KeyModifiers) {
        let Some(Modal::Keybinds(mut editor)) = self.modal.take() else {
            return;
        };
        let keep_open = match editor.stage {
            EditorStage::Capture => {
                self.handle_capture_key(&mut editor, raw_key, code, mods);
                true
            }
            EditorStage::Conflict {
                code: c,
                mods: m,
                displaced,
            } => {
                match (code, mods) {
                    (KeyCode::Esc, _) | (KeyCode::Char('n'), KeyModifiers::NONE) => {
                        editor.stage = EditorStage::Browse;
                        editor.feedback = None;
                    }
                    (KeyCode::Enter, _) | (KeyCode::Char('y'), KeyModifiers::NONE) => {
                        self.confirm_conflict_rebind(&mut editor, c, m, displaced);
                    }
                    _ => {}
                }
                true
            }
            EditorStage::Browse => self.handle_browse_key(&mut editor, code, mods),
        };
        if keep_open {
            self.modal = Some(Modal::Keybinds(editor));
        }
    }

    fn handle_browse_key(
        &mut self,
        editor: &mut Editor,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> bool {
        let plain = mods.is_empty();
        match (code, plain) {
            (KeyCode::Esc | KeyCode::F(2), _) => return false,
            (KeyCode::Up | KeyCode::BackTab, _) => editor.step(-1),
            (KeyCode::Down | KeyCode::Tab, _) => editor.step(1),
            (KeyCode::PageUp, _) => editor.step(-10),
            (KeyCode::PageDown, _) => editor.step(10),
            (KeyCode::Home, _) => {
                editor.selected = 0;
                editor.follow_selection = true;
                editor.feedback = None;
            }
            (KeyCode::End, _) => {
                editor.selected = editor.rows().len().saturating_sub(1);
                editor.follow_selection = true;
                editor.feedback = None;
            }
            (KeyCode::Left, true) => {
                let next = self.keymap.preset().step(-1);
                self.editor_switch_preset(editor, next);
            }
            (KeyCode::Right, true) => {
                let next = self.keymap.preset().step(1);
                self.editor_switch_preset(editor, next);
            }
            (KeyCode::Enter, true) if editor.current_row().is_some() => {
                editor.stage = EditorStage::Capture;
                editor.feedback = None;
            }
            (KeyCode::Delete | KeyCode::Backspace, true) => self.editor_unbind_selected(editor),
            (KeyCode::Char('r'), true) => self.editor_reset_selected(editor),
            (KeyCode::Char('R'), true) => self.editor_reset_all(editor),
            _ => {}
        }
        true
    }

    fn handle_capture_key(
        &mut self,
        editor: &mut Editor,
        raw_key: KeyEvent,
        code: KeyCode,
        mods: KeyModifiers,
    ) {
        if (code, mods) == (KeyCode::Esc, KeyModifiers::NONE) {
            editor.stage = EditorStage::Browse;
            editor.feedback = None;
            return;
        }
        let Some(cur) = editor.current_row() else {
            editor.stage = EditorStage::Browse;
            return;
        };
        let (new_code, new_mods) = match keymap::from_key_event(cur.context, raw_key) {
            Ok(parsed) => parsed,
            Err(err) => {
                editor.feedback = Some((err, true));
                return;
            }
        };
        match self
            .keymap
            .check_rebind(cur.context, cur.action, new_code, new_mods)
        {
            Err(err) => {
                editor.feedback = Some((err, true));
            }
            Ok(Some(displaced)) => {
                editor.stage = EditorStage::Conflict {
                    code: new_code,
                    mods: new_mods,
                    displaced,
                };
                editor.feedback = None;
            }
            Ok(None) => {
                self.commit_rebind(editor, cur.context, cur.action, new_code, new_mods, None);
            }
        }
    }

    fn confirm_conflict_rebind(
        &mut self,
        editor: &mut Editor,
        code: KeyCode,
        mods: KeyModifiers,
        displaced: Action,
    ) {
        let Some(cur) = editor.current_row() else {
            editor.stage = EditorStage::Browse;
            return;
        };
        self.commit_rebind(editor, cur.context, cur.action, code, mods, Some(displaced));
    }

    fn commit_rebind(
        &mut self,
        editor: &mut Editor,
        ctx: Context,
        target: Action,
        code: KeyCode,
        mods: KeyModifiers,
        displaced: Option<Action>,
    ) {
        let actual_displaced = self
            .keymap
            .apply_rebind(ctx, target, code, mods)
            .or(displaced);
        editor.rebuild_rows(&self.keymap);
        editor.stage = EditorStage::Browse;
        let label = format_label(code, mods);
        let desc = keymap::action_spec(target).map_or("action", |s| s.description);
        editor.feedback = Some((format!("Bound {label} to {desc}"), false));

        let mut touched = Vec::new();
        if let Some(other) = actual_displaced {
            touched.push((ctx, other));
        }
        touched.push((ctx, target));
        self.save_keybinds_and_report(editor, None, false, &touched);
    }

    fn editor_unbind_selected(&mut self, editor: &mut Editor) {
        let Some(cur) = editor.current_row() else {
            return;
        };
        match self.keymap.unbind_action(cur.context, cur.action) {
            Ok(()) => {
                editor.rebuild_rows(&self.keymap);
                editor.feedback = Some((format!("Unbound {}", cur.description), false));
                self.save_keybinds_and_report(editor, None, false, &[(cur.context, cur.action)]);
            }
            Err(err) => {
                editor.feedback = Some((err, true));
            }
        }
    }

    fn editor_reset_selected(&mut self, editor: &mut Editor) {
        let Some(cur) = editor.current_row() else {
            return;
        };
        match self.keymap.reset_action(cur.context, cur.action) {
            Ok(touched_actions) => {
                editor.rebuild_rows(&self.keymap);
                editor.feedback = Some((format!("Reset {}", cur.description), false));
                let touched: Vec<_> = touched_actions
                    .into_iter()
                    .map(|a| (cur.context, a))
                    .collect();
                self.save_keybinds_and_report(editor, None, false, &touched);
            }
            Err(err) => {
                editor.feedback = Some((err, true));
            }
        }
    }

    fn editor_reset_all(&mut self, editor: &mut Editor) {
        self.keymap = Keymap::from_preset(Preset::Default);
        editor.rebuild_rows(&self.keymap);
        editor.stage = EditorStage::Browse;
        editor.feedback = Some(("Reset all key bindings to default".to_owned(), false));
        self.save_keybinds_and_report(editor, None, true, &[]);
    }

    fn editor_switch_preset(&mut self, editor: &mut Editor, next: Preset) {
        let warnings = self.keymap.switch_preset(next);
        editor.rebuild_rows(&self.keymap);
        editor.stage = EditorStage::Browse;
        editor.feedback = match warnings.into_iter().next() {
            Some(w) => Some((w, true)),
            None => Some((format!("Preset: {}", next.name()), false)),
        };
        self.save_keybinds_and_report(editor, Some(next), false, &[]);
    }

    fn save_keybinds_and_report(
        &mut self,
        editor: &mut Editor,
        preset: Option<Preset>,
        clear_all: bool,
        touched: &[(Context, Action)],
    ) {
        if let Err(err) = persist_overrides(
            self.config_file.as_deref(),
            &self.keymap,
            preset,
            clear_all,
            touched,
        ) {
            tracing::warn!(?err, "could not save key bindings");
            editor.feedback = Some((format!("Could not save config: {err}"), true));
        }
    }

    pub(super) fn click_keybind_row(&mut self, index: usize) {
        let Some(Modal::Keybinds(mut editor)) = self.modal.take() else {
            return;
        };
        let valid_index = index.min(editor.rows().len().saturating_sub(1));
        if matches!(editor.stage, EditorStage::Browse) && editor.selected == valid_index {
            editor.stage = EditorStage::Capture;
            editor.feedback = None;
        } else {
            editor.selected = valid_index;
            editor.follow_selection = true;
            editor.stage = EditorStage::Browse;
            editor.feedback = None;
        }
        self.modal = Some(Modal::Keybinds(editor));
    }

    pub(super) fn click_keybind_button(&mut self, button: EditorButton) {
        let Some(Modal::Keybinds(mut editor)) = self.modal.take() else {
            return;
        };
        let mut keep_open = true;
        match button {
            EditorButton::PresetPrev => {
                let next = self.keymap.preset().step(-1);
                self.editor_switch_preset(&mut editor, next);
            }
            EditorButton::PresetNext => {
                let next = self.keymap.preset().step(1);
                self.editor_switch_preset(&mut editor, next);
            }
            EditorButton::Rebind => {
                if editor.current_row().is_some() {
                    editor.stage = EditorStage::Capture;
                    editor.feedback = None;
                }
            }
            EditorButton::Unbind => {
                editor.stage = EditorStage::Browse;
                self.editor_unbind_selected(&mut editor);
            }
            EditorButton::Reset => {
                editor.stage = EditorStage::Browse;
                self.editor_reset_selected(&mut editor);
            }
            EditorButton::ResetAll => self.editor_reset_all(&mut editor),
            EditorButton::Close => keep_open = false,
            EditorButton::ConfirmReplace => {
                if let EditorStage::Conflict {
                    code,
                    mods,
                    displaced,
                } = editor.stage
                {
                    self.confirm_conflict_rebind(&mut editor, code, mods, displaced);
                }
            }
            EditorButton::CancelPrompt => {
                editor.stage = EditorStage::Browse;
                editor.feedback = None;
            }
        }
        if keep_open {
            self.modal = Some(Modal::Keybinds(editor));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Pane;
    use crate::config::Config;
    use crate::modal::LoginForm;
    use crate::ui;
    use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn click(column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn wheel(down: bool, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: if down {
                MouseEventKind::ScrollDown
            } else {
                MouseEventKind::ScrollUp
            },
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn redraw(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn find_last(terminal: &Terminal<TestBackend>, needle: &str) -> (u16, u16) {
        let screen = format!("{}", terminal.backend());
        let lines: Vec<_> = screen.lines().collect();
        for (row, line) in lines.iter().enumerate().rev() {
            if let Some(byte) = line.rfind(needle) {
                let col = crate::text::width(&line[..byte]) - 1;
                return (col as u16, row as u16);
            }
        }
        panic!("{needle:?} not on screen:\n{screen}");
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

    fn terminal(w: u16, h: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(w, h)).unwrap()
    }

    #[test]
    fn startup_warnings_and_dynamic_key_labels_everywhere() {
        let cfg = Config::parse(
            r#"
            [keys.global]
            quit = "f10"
            settings = "f5"
            palette = "alt-p"
            focus_composer = "alt-c"
            bogus_action = "f6"
            bad_syntax = "ctrl-c"

            [keys.composer]
            send = "ctrl-s"

            [keys.timeline]
            reply = ["enter", "R"]
            save = "d"
            "#,
        )
        .unwrap();
        let mut app = App::new(&cfg, PathBuf::new(), None);
        assert!(matches!(&app.toast, Some((msg, _)) if msg.contains("Key bindings: 2 problem(s)")));

        let mut term = terminal(100, 30);
        // Clear startup toast so status hints appear on screen.
        app.toast = None;
        app.focus = Pane::Sidebar;
        let screen = redraw(&mut app, &mut term);
        assert!(screen.contains("F5 settings"), "{screen}");
        assert!(screen.contains("Alt-P palette"), "{screen}");
        assert!(screen.contains("press Alt-C to write"), "{screen}");

        // Help overlay reflects overridden keys.
        app.perform(Action::ToggleHelp);
        let help = redraw(&mut app, &mut term);
        assert!(help.contains("F5          Settings"), "{help}");
        assert!(help.contains("F10         Quit"), "{help}");
        assert!(help.contains("Alt-P       Command palette"), "{help}");
        app.on_event(key(KeyCode::Esc));

        // Command palette shows overridden hints.
        app.on_event(key_mod(KeyCode::Char('p'), KeyModifiers::ALT));
        let pal = redraw(&mut app, &mut term);
        assert!(pal.contains("F5"), "{pal}");
        assert!(pal.contains("R"), "{pal}");
        app.on_event(key(KeyCode::Esc));

        // Ctrl-C toast shows rebound Quit key.
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(
            matches!(&app.toast, Some((msg, _)) if msg == "Press F10 to quit"),
            "{:?}",
            app.toast
        );

        // Login modal footer shows rebound Quit key, typing 'q' stays in field, F10 quits.
        app.open_modal(Modal::Login(LoginForm::new(None)));
        let login = redraw(&mut app, &mut term);
        assert!(login.contains("F10 quit"), "{login}");
        app.on_event(key(KeyCode::Char('q')));
        assert!(!app.should_quit);
        app.on_event(key(KeyCode::F(10)));
        assert!(app.should_quit);
    }

    #[test]
    fn vim_preset_navigation_and_composer_precedence() {
        let cfg = Config::parse("preset = \"vim\"\n").unwrap();
        let mut app = App::new(&cfg, PathBuf::new(), None);
        assert!(app.toast.is_none());
        let mut term = terminal(100, 30);
        redraw(&mut app, &mut term);

        // Composer starts focused: plain vim keys MUST type normally.
        for c in "jkghil/?G".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        assert_eq!(app.composer.text(), "jkghil/?G");
        assert_eq!(app.focus, Pane::Composer);

        // Esc in composer moves to timeline without eating draft.
        app.on_event(key(KeyCode::Esc));
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.composer.text(), "jkghil/?G");

        // Timeline: g (JumpToOldest), G (JumpToNewest), k (SelectOlder), j (SelectNewer -> Composer).
        app.on_event(key(KeyCode::Char('g')));
        assert!(app.timeline.scroll > 0);
        app.on_event(key(KeyCode::Char('G')));
        assert_eq!(app.timeline.scroll, 0);
        let newest = app.timeline.selected.expect("newest selected");
        app.on_event(key(KeyCode::Char('k')));
        assert!(app.timeline.selected < Some(newest));
        app.on_event(key(KeyCode::Char('G')));
        app.on_event(key(KeyCode::Char('j')));
        assert_eq!(app.focus, Pane::Composer);

        // Return to Timeline, jump to Sidebar with h.
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Char('h')));
        assert_eq!(app.focus, Pane::Sidebar);

        // Sidebar: j/k/G/g/l/i and `/`.
        let count = app.visible_rooms().len();
        app.sidebar.cursor = 0;
        app.on_event(key(KeyCode::Char('j')));
        assert_eq!(app.sidebar.cursor, 1);
        app.on_event(key(KeyCode::Char('k')));
        assert_eq!(app.sidebar.cursor, 0);
        app.on_event(key(KeyCode::Char('G')));
        assert_eq!(app.sidebar.cursor, count - 1);
        app.on_event(key(KeyCode::Char('g')));
        assert_eq!(app.sidebar.cursor, 0);
        app.on_event(key(KeyCode::Char('/')));
        assert!(matches!(app.modal, Some(Modal::Search(_))));
        app.on_event(key(KeyCode::Esc));
        app.on_event(key(KeyCode::Char('l')));
        assert_eq!(app.focus, Pane::Timeline);
        app.on_event(key(KeyCode::Char('i')));
        assert_eq!(app.focus, Pane::Composer);
    }

    #[test]
    fn keybinds_editor_capture_conflict_quit_rail_mouse_and_persistence() {
        let dir =
            std::env::temp_dir().join(format!("matrix-rat-keybinds-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(&file, "# keep comment\ntheme = \"nord\"\n").unwrap();

        let cfg = Config::load(&file).unwrap();
        let mut app = App::new(&cfg, PathBuf::new(), None);
        app.config_file = Some(file.clone());
        let mut term = terminal(100, 30);

        // F1 still opens plain Help.
        app.on_event(key(KeyCode::F(1)));
        assert!(app.help_open);
        app.on_event(key(KeyCode::Esc));
        assert!(!app.help_open);

        // Open Settings -> click "Key bindings" to open the editor.
        app.perform(Action::OpenSettings);
        redraw(&mut app, &mut term);
        let (col, row) = find(&term, "Key bindings");
        app.on_event(click(col, row));
        assert!(matches!(app.modal, Some(Modal::Keybinds(_))));
        assert!(!app.help_open);
        let screen = redraw(&mut app, &mut term);
        assert!(screen.contains("Preset  ◂ default ▸"), "{screen}");

        // Row 0 is Quit: trying to unbind it is refused by the Quit rail.
        app.on_event(key(KeyCode::Delete));
        let screen = redraw(&mut app, &mut term);
        assert!(screen.contains("Quit must keep at least one"), "{screen}");
        assert_eq!(
            app.keymap.lookup(
                Context::Global,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            Some(Action::Quit)
        );

        // Select "Emoji picker" via click, then click again to enter Capture.
        let (col, row) = find(&term, "Emoji picker");
        app.on_event(click(col, row));
        redraw(&mut app, &mut term);
        let (col, row) = find(&term, "Emoji picker");
        app.on_event(click(col, row));
        let cap = redraw(&mut app, &mut term);
        assert!(cap.contains("<press a key>"), "{cap}");

        // Ctrl-C is rejected inside capture without closing capture; Ctrl-Q is refused without quitting.
        app.on_event(key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.should_quit);
        let cap = redraw(&mut app, &mut term);
        assert!(cap.contains("Ctrl-C is reserved"), "{cap}");

        app.on_event(key_mod(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(!app.should_quit);
        let cap = redraw(&mut app, &mut term);
        assert!(cap.contains("only binding for Quit"), "{cap}");

        // Press F2 (used by Settings in Global) -> enters Conflict prompt BEFORE replacing.
        app.on_event(key(KeyCode::F(2)));
        let conflict = redraw(&mut app, &mut term);
        assert!(conflict.contains("is used by Settings"), "{conflict}");
        assert!(conflict.contains("Replace"), "{conflict}");

        // Cancel conflict preserves both bindings.
        let (cc, cr) = find(&term, "Cancel");
        app.on_event(click(cc, cr));
        redraw(&mut app, &mut term);
        assert_eq!(
            app.keymap.lookup(
                Context::Global,
                KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)
            ),
            Some(Action::OpenSettings)
        );
        assert_eq!(
            app.keymap.lookup(
                Context::Global,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT)
            ),
            Some(Action::OpenEmoji)
        );

        // Now capture again -> F2 -> Replace.
        let (rc, rr) = find_last(&term, " Rebind ");
        app.on_event(click(rc, rr));
        app.on_event(key(KeyCode::F(2)));
        redraw(&mut app, &mut term);
        let (rc, rr) = find_last(&term, " Replace ");
        app.on_event(click(rc, rr));
        redraw(&mut app, &mut term);
        assert_eq!(
            app.keymap.lookup(
                Context::Global,
                KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)
            ),
            Some(Action::OpenEmoji)
        );
        assert_eq!(app.keymap.label_for(Action::OpenSettings), None);

        // Reloading config.toml restores exact same keymap (Emoji = F2, Settings = unbound) and keeps comments.
        let raw_text = std::fs::read_to_string(&file).unwrap();
        assert!(raw_text.contains("# keep comment"), "{raw_text}");
        let reloaded = Config::load(&file).unwrap();
        assert_eq!(reloaded.theme, "nord");
        let (reloaded_km, warnings) = Keymap::from_config(&reloaded);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            reloaded_km.lookup(
                Context::Global,
                KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)
            ),
            Some(Action::OpenEmoji)
        );
        assert_eq!(reloaded_km.label_for(Action::OpenSettings), None);

        // Click Reset on selected row restores Emoji to Alt-E; click preset `▸` switches to vim.
        let (rc, rr) = find_last(&term, " Reset ");
        app.on_event(click(rc, rr));
        assert_eq!(
            app.keymap.lookup(
                Context::Global,
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT)
            ),
            Some(Action::OpenEmoji)
        );
        let (pc, pr) = find(&term, "▸");
        app.on_event(click(pc, pr));
        assert_eq!(app.keymap.preset(), Preset::Vim);
        let vim_text = std::fs::read_to_string(&file).unwrap();
        assert!(vim_text.contains("preset = \"vim\""), "{vim_text}");

        // Wheel scroll works cleanly inside the editor.
        redraw(&mut app, &mut term);
        let scroll_before = match &app.modal {
            Some(Modal::Keybinds(e)) => e.scroll,
            _ => panic!("expected editor"),
        };
        app.on_event(wheel(true, 40, 15));
        redraw(&mut app, &mut term);
        if let Some(Modal::Keybinds(e)) = &app.modal {
            assert!(e.scroll > scroll_before);
        }

        // Reset all restores default preset & clears [keys]/preset from config.toml while keeping comment.
        let (ac, ar) = find_last(&term, " Reset all ");
        app.on_event(click(ac, ar));
        assert_eq!(app.keymap.preset(), Preset::Default);
        assert_eq!(
            app.keymap.lookup(
                Context::Global,
                KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)
            ),
            Some(Action::OpenSettings)
        );
        let cleared = std::fs::read_to_string(&file).unwrap();
        assert!(cleared.contains("# keep comment"), "{cleared}");
        assert!(!cleared.contains("[keys"), "{cleared}");
        assert!(!cleared.contains("preset"), "{cleared}");

        // Close button closes modal.
        let (cc, cr) = find(&term, " Close ");
        app.on_event(click(cc, cr));
        assert!(app.modal.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
