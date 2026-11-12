//! Application state and the event loop. A placeholder screen for now; the real UI
//! arrives with milestone M1.

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph};

use crate::terminal::TerminalGuard;

#[derive(Debug, Default)]
pub struct App {
    should_quit: bool,
    dirty: bool,
}

impl App {
    fn new() -> Self {
        Self {
            should_quit: false,
            dirty: true,
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Resize(..) | Event::FocusGained => self.dirty = true,
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if is_quit(key) {
            self.should_quit = true;
        }
    }

    fn draw(&self, frame: &mut Frame) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" matrix-rat ");
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());

        let [_, middle, _] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Fill(1),
        ])
        .areas(inner);
        let text = vec![
            Line::from(Span::styled(
                concat!("matrix-rat ", env!("CARGO_PKG_VERSION")),
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Line::from("the interface arrives with the next milestone"),
            Line::from("Ctrl-Q to quit"),
        ];
        frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), middle);
    }
}

fn is_quit(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
}

pub async fn run() -> Result<()> {
    let guard = TerminalGuard::enter()?;
    // Terminal queries (image protocol, background colour) go here, before the event stream.
    let mut terminal = guard.activate()?;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut app = App::new();

    while !app.should_quit {
        if app.dirty {
            terminal.draw(|frame| app.draw(frame))?;
            app.dirty = false;
        }
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(event)) => app.on_event(event),
                Some(Err(err)) => return Err(err.into()),
                None => break,
            },
            _ = tick.tick() => {}
            _ = tokio::signal::ctrl_c() => app.should_quit = true,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn quit_keys() {
        let mut app = App::new();
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(!app.should_quit);
        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(app.should_quit);
    }

    /// Proves the Matrix SDK links and its client can be constructed offline.
    #[tokio::test]
    async fn sdk_client_builds() {
        let client = matrix_sdk::Client::builder()
            .homeserver_url("http://localhost:1")
            .build()
            .await;
        assert!(client.is_ok());
        let _ = std::any::type_name::<matrix_sdk_ui::sync_service::SyncService>();
    }

    #[test]
    fn placeholder_renders() {
        let mut terminal = Terminal::new(TestBackend::new(50, 9)).unwrap();
        let app = App::new();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen = format!("{}", terminal.backend());
        assert!(screen.contains("matrix-rat"));
        assert!(screen.contains("Ctrl-Q to quit"));
    }
}
