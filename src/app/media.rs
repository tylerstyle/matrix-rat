//! Pictures in the app: fetching what the timeline shows, and the viewer.

use std::io::Write;

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::*;
use crate::images::{EncodeDone, Graphics, ImageStore};

impl App {
    /// Sets up pictures for the real terminal. The receiver delivers viewer pictures
    /// that were prepared on another thread; pass them to [`App::on_image_encoded`].
    pub(super) fn start_images(&mut self, graphics: Graphics) -> UnboundedReceiver<EncodeDone> {
        let (sender, receiver) = unbounded_channel();
        self.images = ImageStore::new(graphics);
        self.images.set_encoder(sender);
        receiver
    }

    pub(super) fn on_image_encoded(&mut self, done: EncodeDone) {
        self.images.on_encoded(done);
        self.dirty = true;
    }

    /// Hands the picture requests of the last frame to the worker.
    pub(super) fn send_image_requests(&mut self) {
        for request in self.images.take_requests() {
            self.send_matrix(MatrixCmd::FetchImage {
                key: request.key,
                media: request.media,
                size: request.size,
                full: request.full,
            });
        }
    }

    /// Tells the terminal to free the pictures that are no longer shown (`all`: every
    /// picture, before quitting). Written between frames, never during a draw.
    pub(super) fn flush_images(&mut self, all: bool) {
        let sequence = self.images.cleanup_sequence(all);
        if !sequence.is_empty() {
            let mut out = std::io::stdout();
            let _ = out.write_all(sequence.as_bytes());
            let _ = out.flush();
        }
    }

    /// Opens the viewer for the picture of a message. False if it has none.
    pub(super) fn open_image(&mut self, index: usize) -> bool {
        let Some(info) = self.messages().get(index).and_then(|m| m.image.clone()) else {
            return false;
        };
        if !self.images.enabled() {
            self.show_toast("Pictures are turned off in the configuration");
            return true;
        }
        self.help_open = false;
        self.images.open_viewer(&info);
        // The request goes out with the next frame.
        self.dirty = true;
        true
    }

    /// A click on a picture selects its message and opens the viewer.
    pub(super) fn click_image(&mut self, index: usize) {
        self.set_focus(Pane::Timeline);
        if self.selectable(index) {
            self.timeline.selected = Some(index);
        }
        self.open_image(index);
    }

    pub(super) fn on_viewer_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        match (code, mods) {
            (KeyCode::Char('q'), KeyModifiers::CONTROL) => self.should_quit = true,
            (KeyCode::Char('z'), KeyModifiers::CONTROL) => self.suspend_requested = true,
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                if !self.save_attachment(false) {
                    self.show_toast("This picture cannot be saved");
                }
            }
            (KeyCode::Esc | KeyCode::Enter, _)
            | (KeyCode::Char('q' | 'o' | ' '), KeyModifiers::NONE) => self.images.close_viewer(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::testing::{decoded, kitty, remote_image};
    use crate::model::{ImageInfo, RoomKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use tokio::sync::mpsc::UnboundedReceiver;

    const KEY: &str = "mxc://example.org/cat";

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

    fn text(id: &str, body: &str) -> Message {
        Message {
            id: id.into(),
            item: Some(ItemRef::Event(format!("$ev{id}"))),
            sender: "alice".into(),
            time: "10:00".into(),
            day: "Today".into(),
            body: body.into(),
            ..Message::default()
        }
    }

    fn picture(id: &str, info: ImageInfo) -> Message {
        Message {
            body: format!("🖼  {}", info.name),
            image: Some(info),
            ..text(id, "")
        }
    }

    /// An app with one room, connected to a fake worker, showing `messages`.
    fn app_with(
        graphics: Graphics,
        messages: Vec<Message>,
    ) -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let mut app = App::demo();
        let (handle, commands) = MatrixHandle::for_test();
        app.matrix = Some(handle);
        app.sync = Some(SyncStatus::Synced);
        app.images = ImageStore::new(graphics);
        app.rooms = vec![Room::new("!a", "Alpha", RoomKind::Room)];
        app.active = 0;
        app.rooms[0].messages = messages;
        app.timeline.reached_start = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        frame(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    /// One turn of the event loop's drawing.
    fn frame(app: &mut App, terminal: &mut Terminal<TestBackend>) {
        terminal.draw(|f| ui::draw(f, app)).unwrap();
        app.after_draw();
    }

    fn fetches(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if matches!(command, MatrixCmd::FetchImage { .. }) {
                seen.push(format!("{command:?}"));
            }
        }
        seen
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        format!("{}", terminal.backend())
    }

    /// Row of the first screen line containing `needle`.
    fn row_of(terminal: &Terminal<TestBackend>, needle: &str) -> usize {
        screen(terminal)
            .lines()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", screen(terminal)))
    }

    /// Cells that hold part of a half-block picture, row by row. The test pictures
    /// change colour from every pixel row to the next, so each cell is a half block.
    fn picture_cells(buffer: &Buffer) -> Vec<(u16, u16)> {
        let mut cells = Vec::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if matches!(buffer[(x, y)].symbol(), "▀" | "▄") {
                    cells.push((x, y));
                }
            }
        }
        cells
    }

    fn loaded(width: u32, height: u32) -> MatrixEvent {
        MatrixEvent::Image {
            key: KEY.into(),
            full: false,
            result: Ok(decoded(width, height)),
        }
    }

    #[test]
    fn placeholder_then_picture_without_moving_anything() {
        let (mut app, mut terminal, mut commands) = app_with(
            Graphics::halfblocks(),
            vec![
                text("1", "before"),
                picture("2", remote_image(KEY, Some((320, 200)))),
                text("3", "after"),
            ],
        );
        // The space is kept from the start, framed and named, and the file is asked for
        // exactly once however often the screen is drawn.
        let before = row_of(&terminal, "before");
        let after = row_of(&terminal, "after");
        let label = row_of(&terminal, "cat.png");
        assert_eq!(after - before, 11, "{}", screen(&terminal));
        assert!(before < label && label < after);
        // The frame of the placeholder starts right under the message before.
        let framed = screen(&terminal)
            .lines()
            .nth(before + 1)
            .unwrap()
            .to_owned();
        assert!(framed.contains('╭') && framed.contains('╮'), "{framed}");
        assert!(picture_cells(terminal.backend().buffer()).is_empty());
        frame(&mut app, &mut terminal);
        assert_eq!(fetches(&mut commands), ["FetchImage(full: false)"]);

        // The indicator moves with the clock.
        let first = screen(&terminal);
        app.dirty = false;
        app.on_tick();
        assert!(app.dirty);
        frame(&mut app, &mut terminal);
        assert_ne!(first, screen(&terminal));

        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        assert_eq!(row_of(&terminal, "before"), before);
        assert_eq!(row_of(&terminal, "after"), after);
        assert!(!screen(&terminal).contains("cat.png"));
        // 320x200 pixels at 10x20 per cell: 32 columns, 10 rows, two columns in.
        let cells = picture_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 32 * 10);
        let (left, top) = cells[0];
        assert_eq!(top as usize, before + 1);
        assert!(
            cells
                .iter()
                .all(|(x, y)| { (left..left + 32).contains(x) && (top..top + 10).contains(y) })
        );
        // Nothing left to animate, nothing more to fetch.
        app.dirty = false;
        app.on_tick();
        assert!(!app.dirty);
        frame(&mut app, &mut terminal);
        assert!(fetches(&mut commands).is_empty());
    }

    #[test]
    fn a_picture_that_cannot_be_loaded_becomes_one_line() {
        let (mut app, mut terminal, _commands) = app_with(
            Graphics::halfblocks(),
            vec![
                picture("1", remote_image(KEY, Some((320, 200)))),
                text("2", "after"),
            ],
        );
        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: false,
            result: Err("could not be loaded".into()),
        });
        frame(&mut app, &mut terminal);
        let line = row_of(&terminal, "cat.png — could not be loaded");
        assert_eq!(row_of(&terminal, "after"), line + 1);
    }

    #[test]
    fn caption_goes_under_the_picture_and_off_shows_text_only() {
        let mut info = remote_image(KEY, Some((320, 200)));
        info.caption = Some("Look at this".into());
        let message = Message {
            body: "🖼  cat.png\nLook at this".into(),
            ..picture("1", info)
        };
        let (mut app, mut terminal, _commands) =
            app_with(Graphics::halfblocks(), vec![message.clone()]);
        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        let caption = row_of(&terminal, "Look at this");
        let cells = picture_cells(terminal.backend().buffer());
        assert_eq!(cells.last().unwrap().1 as usize + 1, caption);
        assert!(!screen(&terminal).contains("cat.png"));

        // Off: the file name and the caption as text, nothing fetched, nothing to view.
        let (mut app, mut terminal, mut commands) = app_with(Graphics::off(), vec![message]);
        assert_eq!(
            row_of(&terminal, "cat.png") + 1,
            row_of(&terminal, "Look at this")
        );
        assert!(fetches(&mut commands).is_empty());
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('o')));
        frame(&mut app, &mut terminal);
        assert!(!app.images.viewer_open());
        assert!(screen(&terminal).contains("Pictures are turned off"));
        assert!(fetches(&mut commands).is_empty());
    }

    #[test]
    fn viewer_opens_by_key_and_closes_with_escape() {
        let (mut app, mut terminal, mut commands) = app_with(
            Graphics::halfblocks(),
            vec![
                text("1", "before"),
                picture("2", remote_image(KEY, Some((320, 200)))),
            ],
        );
        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        fetches(&mut commands);

        // A message without a picture has nothing to open.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        assert_eq!(app.timeline.selected, Some(0));
        app.on_event(key(KeyCode::Char('o')));
        assert!(!app.images.viewer_open());

        app.on_event(key(KeyCode::Down));
        assert_eq!(app.timeline.selected, Some(1));
        app.on_event(key(KeyCode::Char('o')));
        assert!(app.images.viewer_open());
        frame(&mut app, &mut terminal);
        // The whole file is fetched; meanwhile the viewer says so and covers the rest.
        assert_eq!(fetches(&mut commands), ["FetchImage(full: true)"]);
        let shown = screen(&terminal);
        assert!(shown.contains("Loading cat.png"), "{shown}");
        assert!(shown.contains("cat.png · 320×200 · 2.0 kB"), "{shown}");
        assert!(!shown.contains("before"));
        assert!(picture_cells(terminal.backend().buffer()).is_empty());
        // Keys belong to the viewer while it is open.
        app.on_event(key(KeyCode::Char('r')));
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        assert!(app.images.viewer_open());

        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: true,
            result: Ok(decoded(1600, 1000)),
        });
        frame(&mut app, &mut terminal);
        let shown = screen(&terminal);
        assert!(!shown.contains("Loading"));
        assert!(shown.contains("cat.png · 1600×1000 · 2.0 kB"), "{shown}");
        // As large as fits into 78x28 cells: limited by the height, 1600x1000 at
        // 10x20 per cell is 78x25 after scaling by the width.
        let cells = picture_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 78 * 25, "{shown}");

        app.on_event(key(KeyCode::Esc));
        assert!(!app.images.viewer_open());
        frame(&mut app, &mut terminal);
        assert!(screen(&terminal).contains("before"));
        // Back in the timeline, with the message still selected.
        assert_eq!(app.focus, Pane::Timeline);
        assert_eq!(app.timeline.selected, Some(1));
        assert_eq!(picture_cells(terminal.backend().buffer()).len(), 32 * 10);
    }

    #[test]
    fn viewer_opens_and_closes_by_click_and_reports_failure() {
        let (mut app, mut terminal, mut commands) = app_with(
            Graphics::halfblocks(),
            vec![picture("1", remote_image(KEY, Some((320, 200))))],
        );
        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        fetches(&mut commands);
        let (x, y) = picture_cells(terminal.backend().buffer())[40];
        app.on_event(click(x, y));
        assert!(app.images.viewer_open());
        assert_eq!(app.timeline.selected, Some(0));
        frame(&mut app, &mut terminal);
        assert_eq!(fetches(&mut commands), ["FetchImage(full: true)"]);

        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: true,
            result: Err("too large to show (34 MB)".into()),
        });
        frame(&mut app, &mut terminal);
        assert!(screen(&terminal).contains("cat.png: too large to show (34 MB)"));

        // The wheel does not scroll what is behind; a click anywhere closes.
        app.on_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 40,
            row: 10,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.timeline.scroll, 0);
        app.on_event(click(40, 10));
        assert!(!app.images.viewer_open());
    }

    #[test]
    fn clicking_a_failed_picture_tries_again() {
        let (mut app, mut terminal, mut commands) = app_with(
            Graphics::halfblocks(),
            vec![picture("1", remote_image(KEY, Some((320, 200))))],
        );
        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: false,
            result: Err("could not be loaded".into()),
        });
        frame(&mut app, &mut terminal);
        fetches(&mut commands);
        let row = row_of(&terminal, "could not be loaded") as u16;
        app.on_event(click(40, row));
        assert!(app.images.viewer_open());
        frame(&mut app, &mut terminal);
        assert_eq!(fetches(&mut commands), ["FetchImage(full: true)"]);
        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: true,
            result: Ok(decoded(320, 200)),
        });
        app.on_event(key(KeyCode::Esc));
        frame(&mut app, &mut terminal);
        assert!(!screen(&terminal).contains("could not be loaded"));
        assert_eq!(picture_cells(terminal.backend().buffer()).len(), 32 * 10);
    }

    #[test]
    fn pictures_scrolled_half_out_are_cropped_and_only_near_ones_are_fetched() {
        let mut messages: Vec<Message> = (0..60)
            .map(|n| text(&format!("t{n}"), &format!("line {n}")))
            .collect();
        // One far up, one a little above the newest screenful.
        messages[2] = picture(
            "far",
            remote_image("mxc://example.org/far", Some((320, 200))),
        );
        messages[30] = picture("near", remote_image(KEY, Some((320, 200))));
        let (mut app, mut terminal, mut commands) = app_with(Graphics::halfblocks(), messages);
        // Not on screen yet, but within a page of it: fetched ahead. The far one is not.
        assert!(picture_cells(terminal.backend().buffer()).is_empty());
        assert_eq!(fetches(&mut commands), ["FetchImage(full: false)"]);
        assert!(app.images.is_loading(KEY));
        app.on_matrix(loaded(320, 200));
        let timeline_rows = 1..row_of(&terminal, "Type a message") as u16 - 1;

        // Scroll up row by row: the picture comes in over the top edge, is shown in
        // full, and leaves over the bottom edge.
        let mut seen = Vec::new();
        let mut frames = Vec::new();
        for _ in 0..45 {
            app.timeline.scroll += 1;
            frame(&mut app, &mut terminal);
            let buffer = terminal.backend().buffer();
            let cells = picture_cells(buffer);
            assert_eq!(cells.len() % 32, 0);
            // Never outside the timeline.
            assert!(cells.iter().all(|(_, y)| timeline_rows.contains(y)));
            seen.push(cells.len() / 32);
            // The colours down the left edge tell which rows of the picture these are.
            let left: Vec<_> = cells
                .iter()
                .step_by(32)
                .map(|(x, y)| (buffer[(*x, *y)].fg, buffer[(*x, *y)].bg))
                .collect();
            frames.push(left);
        }
        assert!(
            seen.windows(2).all(|pair| pair[0].abs_diff(pair[1]) <= 1),
            "{seen:?}"
        );
        let full = seen
            .iter()
            .position(|rows| *rows == 10)
            .expect("shown in full");
        let entering = seen.iter().position(|rows| *rows == 4).unwrap();
        let leaving = seen.iter().rposition(|rows| *rows == 4).unwrap();
        assert!(entering < full && full < leaving, "{seen:?}");
        assert_eq!(seen.last(), Some(&0), "{seen:?}");
        // Cut by the top edge, its lower rows are shown; cut by the bottom edge, its
        // upper rows.
        assert_eq!(frames[entering][..], frames[full][6..]);
        assert_eq!(frames[leaving][..], frames[full][..4]);
        assert_ne!(frames[entering], frames[leaving]);

        // Only far up is the other picture wanted.
        app.timeline.scroll = usize::MAX;
        frame(&mut app, &mut terminal);
        assert_eq!(fetches(&mut commands), ["FetchImage(full: false)"]);
    }

    #[test]
    fn the_view_stays_put_when_a_picture_below_turns_out_smaller() {
        let mut messages: Vec<Message> = (0..60)
            .map(|n| text(&format!("t{n}"), &format!("line {n}")))
            .collect();
        // The sender did not say how large the picture is.
        messages[57] = picture("p", remote_image(KEY, None));
        let (mut app, mut terminal, _commands) = app_with(Graphics::halfblocks(), messages);
        // Scrolled up so that the picture is just below the view, but near enough to
        // have been asked for.
        app.timeline.scroll = 14;
        frame(&mut app, &mut terminal);
        let before = (row_of(&terminal, "line 40"), row_of(&terminal, "line 52"));
        assert!(!screen(&terminal).contains("cat.png"));
        assert!(!screen(&terminal).contains("line 53"));
        assert!(app.images.is_loading(KEY));

        // Much flatter than the space that was kept for it.
        app.on_matrix(loaded(400, 40));
        frame(&mut app, &mut terminal);
        let after = (row_of(&terminal, "line 40"), row_of(&terminal, "line 52"));
        assert_eq!(before, after);
        assert!(!screen(&terminal).contains("line 53"));
        // At the bottom the picture now takes two rows instead of eight.
        app.timeline.scroll = 0;
        frame(&mut app, &mut terminal);
        assert_eq!(picture_cells(terminal.backend().buffer()).len(), 40 * 2);
    }

    #[test]
    fn switching_rooms_resizing_and_themes_leave_no_picture_behind() {
        let (mut app, mut terminal, _commands) = app_with(
            Graphics::halfblocks(),
            vec![picture("1", remote_image(KEY, Some((320, 200))))],
        );
        app.rooms.push(Room::new("!b", "Beta", RoomKind::Room));
        app.rooms[1].messages = vec![text("b1", "nothing to see")];
        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        assert_eq!(picture_cells(terminal.backend().buffer()).len(), 320);

        app.perform(Action::CycleTheme);
        frame(&mut app, &mut terminal);
        assert_eq!(picture_cells(terminal.backend().buffer()).len(), 320);

        // Narrower than the picture: it is fitted to the pane again.
        terminal.backend_mut().resize(36, 20);
        app.on_event(Event::Resize(36, 20));
        frame(&mut app, &mut terminal);
        let cells = picture_cells(terminal.backend().buffer());
        assert!(!cells.is_empty() && cells.len() < 320);
        assert!(cells.iter().all(|(x, _)| *x < 35));

        terminal.backend_mut().resize(80, 30);
        app.on_event(Event::Resize(80, 30));
        app.open_room(1);
        frame(&mut app, &mut terminal);
        assert!(screen(&terminal).contains("nothing to see"));
        assert!(picture_cells(terminal.backend().buffer()).is_empty());
    }

    /// The symbol of the first cell of a kitty picture, if one is on screen.
    fn kitty_cells(buffer: &Buffer) -> Vec<(u16, u16, String)> {
        let mut cells = Vec::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let symbol = buffer[(x, y)].symbol();
                if symbol.contains('\u{10EEEE}') {
                    cells.push((x, y, symbol.to_owned()));
                }
            }
        }
        cells
    }

    #[test]
    fn kitty_pictures_are_sent_once_and_live_in_cells() {
        let (mut app, mut terminal, _commands) = app_with(
            kitty(),
            vec![
                text("0", "before"),
                picture("1", remote_image(KEY, Some((320, 200)))),
            ],
        );
        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 32 * 10);
        // The first cell carries the picture itself to the terminal, once.
        assert!(cells[0].2.starts_with("\x1b_G"));
        assert!(
            cells[1..]
                .iter()
                .all(|(_, _, symbol)| !symbol.contains('\x1b'))
        );
        frame(&mut app, &mut terminal);
        let again = kitty_cells(terminal.backend().buffer());
        assert_eq!(again.len(), 320);
        assert!(again.iter().all(|(_, _, symbol)| !symbol.contains('\x1b')));
        assert!(app.images.cleanup_sequence(false).is_empty());

        // A popup on top simply replaces the cells it covers.
        app.perform(Action::ToggleHelp);
        frame(&mut app, &mut terminal);
        assert!(kitty_cells(terminal.backend().buffer()).len() < 320);
        app.perform(Action::ToggleHelp);

        // Scrolled half out: the remaining rows, and nothing is sent again.
        for n in 0..40 {
            app.rooms[0]
                .messages
                .push(text(&format!("x{n}"), &format!("more {n}")));
        }
        app.timeline_rev += 1;
        app.timeline.scroll = usize::MAX;
        frame(&mut app, &mut terminal);
        // Day line, name and "before" are above the picture: this hides four of its rows.
        app.timeline.scroll -= 7;
        frame(&mut app, &mut terminal);
        let cropped = kitty_cells(terminal.backend().buffer());
        assert_eq!(cropped.len(), 32 * 6, "{}", screen(&terminal));
        assert!(
            cropped
                .iter()
                .all(|(_, _, symbol)| !symbol.contains('\x1b'))
        );

        // After a resize the terminal may have forgotten it: sent again, old one freed.
        app.timeline.scroll = usize::MAX;
        app.on_event(Event::Resize(80, 30));
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert!(cells[0].2.starts_with("\x1b_G"));
        assert!(
            app.images
                .cleanup_sequence(false)
                .starts_with("\x1b_Ga=d,d=I,i=")
        );
    }

    #[test]
    fn a_kitty_picture_arriving_under_a_popup_is_sent_when_the_popup_is_gone() {
        let (mut app, mut terminal, _commands) = app_with(
            kitty(),
            vec![picture("1", remote_image(KEY, Some((320, 200))))],
        );
        app.perform(Action::ToggleHelp);
        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        // Nothing was transmitted into cells the popup might cover.
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());
        app.perform(Action::ToggleHelp);
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 320);
        assert!(cells[0].2.starts_with("\x1b_G"));
    }

    #[test]
    fn a_failure_dropped_from_the_cache_is_tried_again() {
        let (mut app, mut terminal, mut commands) = app_with(
            Graphics::halfblocks(),
            vec![
                picture("1", remote_image(KEY, Some((320, 200)))),
                text("2", "after"),
            ],
        );
        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: false,
            result: Err("could not be loaded".into()),
        });
        frame(&mut app, &mut terminal);
        let line = row_of(&terminal, "could not be loaded");
        assert_eq!(row_of(&terminal, "after"), line + 1);
        fetches(&mut commands);

        // Many other pictures come and go until the cache drops the failure.
        for n in 0..300 {
            let other = remote_image(&format!("mxc://example.org/other{n}"), None);
            app.images.begin_frame();
            app.images.request(&other);
            app.images.on_loaded(&other.key, false, Err("no".into()));
        }
        app.images.take_requests();
        assert!(!app.images.is_loading(KEY));
        // The row does not keep saying "could not be loaded" for a picture nobody
        // remembers failing: its space is back and it is asked for again.
        frame(&mut app, &mut terminal);
        assert!(!screen(&terminal).contains("could not be loaded"));
        assert!(row_of(&terminal, "after") - row_of(&terminal, "cat.png") > 2);
        assert_eq!(fetches(&mut commands), ["FetchImage(full: false)"]);
    }

    #[test]
    fn logging_out_forgets_all_pictures_and_closes_the_viewer() {
        let (mut app, mut terminal, mut commands) = app_with(
            kitty(),
            vec![picture("1", remote_image(KEY, Some((320, 200))))],
        );
        app.on_matrix(loaded(320, 200));
        frame(&mut app, &mut terminal);
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('o')));
        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: true,
            result: Ok(decoded(640, 400)),
        });
        frame(&mut app, &mut terminal);
        assert!(app.images.viewer_open());
        assert!(!kitty_cells(terminal.backend().buffer()).is_empty());
        app.images.cleanup_sequence(false);
        fetches(&mut commands);

        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        assert!(!app.images.viewer_open());
        frame(&mut app, &mut terminal);
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());
        // The terminal is told to free both pictures it was sent.
        assert_eq!(
            app.images
                .cleanup_sequence(false)
                .matches("a=d,d=I")
                .count(),
            2
        );

        // The next account starts from nothing: a result that was still on its way
        // is ignored, and the same picture would be fetched anew.
        app.on_matrix(loaded(320, 200));
        app.on_matrix(MatrixEvent::Image {
            key: KEY.into(),
            full: true,
            result: Ok(decoded(640, 400)),
        });
        assert!(!app.images.viewer_open());
        let info = remote_image(KEY, None);
        assert_eq!(
            app.images.slot(&info, 80),
            crate::images::Slot::Cells(24, 8)
        );
        app.images.begin_frame();
        app.images.request(&info);
        assert_eq!(app.images.take_requests().len(), 1);
    }

    #[test]
    fn image_messages_cannot_be_edited_into_text() {
        let mut message = picture("1", remote_image(KEY, Some((320, 200))));
        message.own = true;
        let (mut app, mut terminal, _commands) = app_with(Graphics::halfblocks(), vec![message]);
        app.on_event(key(KeyCode::Up));
        app.perform(Action::Edit);
        assert_eq!(app.compose_mode, ComposeMode::Normal);
        frame(&mut app, &mut terminal);
        assert!(screen(&terminal).contains("cannot be edited"));
    }

    #[test]
    fn demo_has_a_picture() {
        let mut app = App::demo();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let design = app.rooms.iter().position(|r| r.name == "design").unwrap();
        app.open_room(design);
        frame(&mut app, &mut terminal);
        // Drawn at once, without a server: 480x270 fits into 40x12 as 40x12.
        assert_eq!(picture_cells(terminal.backend().buffer()).len(), 40 * 12);
        insta::assert_snapshot!(screen(&terminal));
        // The viewer works too.
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Up));
        app.on_event(key(KeyCode::Char('o')));
        frame(&mut app, &mut terminal);
        assert!(app.images.viewer_open());
        assert!(screen(&terminal).contains("palette.png · 1280×720"));
        assert!(picture_cells(terminal.backend().buffer()).len() > 40 * 12);
    }
}
