//! Copying a fenced code block of a message: the key and menu entry (`Action::CopyCode`)
//! and the "copy" label on the block itself.
//!
//! Only formatted messages have blocks (`<pre>`, which is what Markdown fences become
//! in Matrix messages, also from other clients). A plain body with backticks is shown
//! as it was written and has none.

use super::App;
use crate::menu::{self, Target};
use crate::rich::CodeBlock;

impl App {
    /// `Action::CopyCode`: the selected message's block, or a choice between several.
    pub(super) fn copy_code_of_selected(&mut self) {
        let Some(index) = self.timeline.selected else {
            self.show_toast("No message is selected");
            return;
        };
        let blocks = self.code_blocks_of(index);
        let mut real = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| !block.text.is_empty());
        match (real.next(), real.next()) {
            (None, _) => self.show_toast("No code block in this message"),
            (Some((at, _)), None) => self.copy_code_block(index, at),
            (Some(_), Some(_)) => {
                let (Some(target), items) = (self.message_ref(index), menu::code_items(&blocks))
                else {
                    return;
                };
                let at = self.message_anchor(index);
                self.show_menu(Target::Message(target), "Copy which code block?", items, at);
            }
        }
    }

    fn code_blocks_of(&self, index: usize) -> Vec<CodeBlock> {
        self.messages()
            .get(index)
            .map(|message| message.code_blocks())
            .unwrap_or_default()
    }

    /// Puts block `block` (its index among the message's blocks) on the clipboard.
    pub(super) fn copy_code_block(&mut self, index: usize, block: usize) {
        let text = self
            .code_blocks_of(index)
            .into_iter()
            .nth(block)
            .map(|block| block.text)
            .filter(|text| !text.is_empty());
        let Some(text) = text else {
            self.show_toast("That code block is gone");
            return;
        };
        let lines = text.lines().count().max(1);
        self.pending_copy = Some(text);
        self.show_toast(match lines {
            1 => "Copied 1 line".to_owned(),
            n => format!("Copied {n} lines"),
        });
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::action::{Action, Pane};
    use crate::modal::Modal;
    use crate::model::Message;

    const ONE: &str = "<p>try this</p><pre><code class=\"language-rust\">fn main() {\n    println!(\"hi\");\n}\n</code></pre>";
    const TWO: &str = "<pre><code class=\"language-sh\">ls -l\n</code></pre><p>or</p>\
        <pre><code>\tindented\nsecond</code></pre>";

    fn message(html: Option<&str>, body: &str) -> Message {
        Message {
            sender: "alice".into(),
            time: "12:00".into(),
            day: "Today".into(),
            body: body.into(),
            html: html.map(str::to_owned),
            ..Message::default()
        }
    }

    fn app_with(messages: Vec<Message>) -> (App, Terminal<TestBackend>) {
        let mut app = App::demo();
        *app.messages_mut().unwrap() = messages;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        app.timeline.selected = Some(0);
        app.set_focus(Pane::Timeline);
        screen(&mut app, &mut terminal);
        (app, terminal)
    }

    fn screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    /// The screen column of the last `needle` in a line of the printed screen (which
    /// is quoted, and has multi-byte frame characters).
    fn column_of(line: &str, needle: &str) -> u16 {
        let line = line.trim_start_matches('"');
        line[..line.rfind(needle).unwrap()].chars().count() as u16
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

    #[test]
    fn one_block_is_copied_at_once_exactly() {
        let (mut app, _) = app_with(vec![message(Some(ONE), "try this")]);
        app.perform(Action::CopyCode);
        assert_eq!(
            app.pending_copy.as_deref(),
            Some("fn main() {\n    println!(\"hi\");\n}")
        );
        assert_eq!(app.toast.as_ref().unwrap().0, "Copied 3 lines");
        assert!(app.modal.is_none());
    }

    #[test]
    fn several_blocks_ask_which() {
        let (mut app, mut terminal) = app_with(vec![message(Some(TWO), "x")]);
        app.on_event(key(KeyCode::Char('C')));
        let Some(Modal::Menu(menu)) = &app.modal else {
            panic!("no chooser: {:?}", app.modal);
        };
        let labels: Vec<_> = menu.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["sh: ls -l", "indented"]);
        assert_eq!(menu.items[1].hint, "2 lines");
        let text = screen(&mut app, &mut terminal);
        assert!(text.contains("Copy which code block?"), "{text}");
        assert!(app.pending_copy.is_none());

        app.on_event(key(KeyCode::Down));
        app.on_event(key(KeyCode::Enter));
        assert!(app.modal.is_none());
        // The tab of the sender stays a tab.
        assert_eq!(app.pending_copy.as_deref(), Some("\tindented\nsecond"));
        assert_eq!(app.toast.as_ref().unwrap().0, "Copied 2 lines");
    }

    #[test]
    fn the_chooser_can_be_left_and_clicked() {
        let (mut app, mut terminal) = app_with(vec![message(Some(TWO), "x")]);
        app.perform(Action::CopyCode);
        app.on_event(key(KeyCode::Esc));
        assert!(app.modal.is_none() && app.pending_copy.is_none());

        app.perform(Action::CopyCode);
        let text = screen(&mut app, &mut terminal);
        let (row, line) = text
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains("sh: ls -l"))
            .unwrap();
        app.on_event(click(column_of(line, "sh:") + 1, row as u16));
        assert_eq!(app.pending_copy.as_deref(), Some("ls -l"));
        assert_eq!(app.toast.as_ref().unwrap().0, "Copied 1 line");
    }

    #[test]
    fn a_message_without_code_says_so() {
        let (mut app, _) = app_with(vec![message(None, "```\nnot a block here\n```")]);
        app.perform(Action::CopyCode);
        assert!(app.pending_copy.is_none());
        assert_eq!(
            app.toast.as_ref().unwrap().0,
            "No code block in this message"
        );
    }

    #[test]
    fn the_label_copies_its_own_block() {
        let (mut app, mut terminal) = app_with(vec![message(Some(TWO), "x")]);
        let text = screen(&mut app, &mut terminal);
        // One label per block, on the block's first line, at the right.
        let labelled: Vec<&str> = text.lines().filter(|l| l.contains("copy")).collect();
        assert_eq!(labelled.len(), 2, "{text}");
        assert!(labelled[0].contains("ls -l"));
        assert!(labelled[1].contains("indented"));
        let (row, line) = text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains("copy"))
            .nth(1)
            .unwrap();
        app.on_event(click(column_of(line, "copy") + 1, row as u16));
        assert_eq!(app.pending_copy.as_deref(), Some("\tindented\nsecond"));
        assert_eq!(app.timeline.selected, Some(0));
    }

    #[test]
    fn there_is_no_label_without_room_or_code() {
        let long = format!("<pre>{}</pre>", "x".repeat(200));
        let (mut app, mut terminal) = app_with(vec![
            message(Some(&long), "x"),
            message(Some("<p>copy me</p>"), "copy me"),
        ]);
        let text = screen(&mut app, &mut terminal);
        // The first line of the long block fills the row: no label. Other text that says
        // "copy" is not one either.
        assert_eq!(text.lines().filter(|l| l.contains("copy")).count(), 1);
        app.timeline.selected = Some(0);
        app.perform(Action::CopyCode);
        assert_eq!(app.pending_copy.as_deref(), Some("x".repeat(200).as_str()));
    }

    #[test]
    fn the_menu_offers_it_only_for_code() {
        let (mut app, _) = app_with(vec![
            message(Some(ONE), "try this"),
            message(Some("<p>hello</p>"), "hello"),
        ]);
        assert!(app.messages()[0].has_code());
        assert!(!app.messages()[1].has_code());
        let items = |app: &mut App, index: usize| {
            app.timeline.selected = Some(index);
            app.perform(Action::ContextMenu);
            let Some(Modal::Menu(menu)) = app.modal.take() else {
                panic!("no menu");
            };
            menu.items.into_iter().map(|i| i.label).collect::<Vec<_>>()
        };
        assert!(items(&mut app, 0).contains(&"Copy code block".to_owned()));
        assert!(!items(&mut app, 1).contains(&"Copy code block".to_owned()));
    }
}
