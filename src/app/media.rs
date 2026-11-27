//! Pictures in the app: fetching what the timeline shows, and the viewer.

use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::*;
use crate::images::{EncodeDone, Graphics, ImageStore};

impl App {
    /// Sets up pictures for the real terminal. The receiver delivers viewer pictures
    /// that were prepared on another thread; pass them to [`App::on_image_encoded`].
    pub(crate) fn start_images(&mut self, graphics: Graphics) -> UnboundedReceiver<EncodeDone> {
        let (sender, receiver) = unbounded_channel();
        self.images = ImageStore::new(graphics);
        self.images.set_encoder(sender);
        receiver
    }

    pub(crate) fn on_image_encoded(&mut self, done: EncodeDone) {
        self.images.on_encoded(done);
        self.dirty = true;
    }

    /// Hands any pending room picture and custom emoji picture requests to the worker
    /// immediately.
    pub(super) fn send_avatar_requests(&mut self) {
        for url in self.images.take_avatar_requests() {
            self.send_matrix(MatrixCmd::FetchAvatar { url });
        }
        for url in self.images.take_emote_requests() {
            self.send_matrix(MatrixCmd::FetchEmotePicture { url });
        }
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
        self.send_avatar_requests();
    }

    /// What tells the terminal to free the pictures that are no longer shown (`all`:
    /// every picture, before quitting), for the event loop to write between frames.
    pub fn image_cleanup(&mut self, all: bool) -> String {
        self.images.cleanup_sequence(all)
    }

    /// Opens the viewer for the picture of a message. False if it has none.
    pub(super) fn open_image(&mut self, index: usize) -> bool {
        let Some(info) = self.messages().get(index).and_then(|m| m.image.clone()) else {
            return false;
        };
        if !self.images.enabled() {
            // No pictures in here: the system's viewer shows it instead.
            self.timeline.selected = Some(index);
            return self.save_attachment(true);
        }
        self.cancel_pending_jump();
        self.help_open = false;
        self.images.open_viewer(&info);
        // The request goes out with the next frame.
        self.dirty = true;
        true
    }

    /// A click on a picture or playable media selects its message and opens/plays it.
    pub(super) fn click_image(&mut self, index: usize) {
        self.cancel_pending_jump();
        self.set_focus(Pane::Timeline);
        if self.selectable(index) {
            self.timeline.selected = Some(index);
        }
        let sound = self.messages().get(index).is_some_and(|m| m.is_sound());
        let audio = self.audio_caps();
        if sound && self.timeline.selected == Some(index) && (audio.can_play || audio.selected) {
            // Played (or stopped) in here, as `x` does; where it cannot be (over SSH,
            // without a player), the system's player as before.
            self.perform(crate::action::Action::PlayAudio);
        } else if self.messages().get(index).is_some_and(|m| m.file.is_some()) {
            self.save_attachment(true);
        } else {
            self.open_image(index);
        }
    }

    pub(super) fn on_viewer_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if let Some(action) = self.keymap.lookup_popup_global(code, mods) {
            #[expect(
                clippy::wildcard_enum_match_arm,
                reason = "lookup_popup_global only returns Action::Quit or Action::Suspend"
            )]
            match action {
                crate::action::Action::Quit => self.should_quit = true,
                crate::action::Action::Suspend => {
                    // As everywhere: the player is its own process group and would
                    // play on while the app is stopped.
                    self.stop_audio();
                    self.stop_recording();
                    self.suspend_requested = true;
                }
                _ => {}
            }
            return;
        }
        if self.keymap.lookup(
            crate::keymap::Context::Timeline,
            crate::input::KeyEvent::new(code, mods),
        ) == Some(crate::action::Action::Save)
        {
            if !self.save_attachment(false) {
                self.show_toast("This picture cannot be saved");
            }
            return;
        }
        if self.keymap.lookup(
            crate::keymap::Context::Timeline,
            crate::input::KeyEvent::new(code, mods),
        ) == Some(crate::action::Action::OpenExternal)
        {
            if !self.save_attachment_for(true, crate::action::Action::OpenExternal) {
                self.show_toast("This picture cannot be opened in another program");
            }
            return;
        }
        match (code, mods) {
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
    use crate::ui;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use tokio::sync::mpsc::UnboundedReceiver;

    const KEY: &str = "mxc://example.org/cat";

    fn key(code: KeyCode) -> InputEvent {
        InputEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(column: u16, row: u16) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
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
                seen.push(command.describe());
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

        // Off: the file name and the caption as text, nothing fetched, no viewer in here;
        // opening hands the picture to the system's viewer instead.
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
        assert!(screen(&terminal).contains("Saving cat.png"));
        let sent: Vec<MatrixCmd> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(
            sent.iter()
                .any(|command| matches!(command, MatrixCmd::SaveFile { open: true, .. })),
            "the picture is saved and opened"
        );
        assert!(
            !sent
                .iter()
                .any(|command| matches!(command, MatrixCmd::FetchImage { .. })),
            "nothing is fetched to be drawn in here"
        );
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
        app.on_event(InputEvent::Mouse(MouseEvent {
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
        app.on_event(InputEvent::Resize(36, 20));
        frame(&mut app, &mut terminal);
        let cells = picture_cells(terminal.backend().buffer());
        assert!(!cells.is_empty() && cells.len() < 320);
        assert!(cells.iter().all(|(x, _)| *x < 35));

        terminal.backend_mut().resize(80, 30);
        app.on_event(InputEvent::Resize(80, 30));
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
        app.on_event(InputEvent::Resize(80, 30));
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

    fn emote_fetches(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::FetchEmotePicture { url } = command {
                seen.push(url);
            }
        }
        seen
    }

    /// A message with custom emoji `names` in its text (their pictures at
    /// `mxc://example.org/<name>`) and a reaction made with `:party:`.
    fn with_emoji(id: &str, names: &[&str]) -> Message {
        let html: String = names
            .iter()
            .map(|name| {
                format!("<img data-mx-emoticon src=\"mxc://example.org/{name}\" alt=\":{name}:\"> ")
            })
            .collect();
        Message {
            html: Some(format!("look {html}here")),
            reactions: vec![crate::model::Reaction {
                key: "mxc://example.org/party".into(),
                count: 3,
                own: false,
            }],
            ..text(id, "look here")
        }
    }

    #[test]
    fn custom_emoji_become_pictures_when_they_arrive() {
        let (mut app, mut terminal, mut commands) =
            app_with(kitty(), vec![with_emoji("1", &["blob"])]);
        // Their names until the pictures are there; both are asked for, once.
        assert!(screen(&terminal).contains("look :blob: here"));
        assert!(screen(&terminal).contains(" :?: 3 "));
        assert_eq!(
            emote_fetches(&mut commands),
            ["mxc://example.org/blob", "mxc://example.org/party"]
        );
        frame(&mut app, &mut terminal);
        assert!(emote_fetches(&mut commands).is_empty());
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());

        // One arrives: the rows are laid out again, two cells kept for it and the
        // picture in them, the data travelling in the first.
        app.on_matrix(MatrixEvent::EmotePicture {
            url: "mxc://example.org/blob".into(),
            result: Ok(decoded(64, 64)),
        });
        frame(&mut app, &mut terminal);
        let row = row_of(&terminal, "look") as u16;
        assert!(!screen(&terminal).contains(":blob:"));
        let buffer = terminal.backend().buffer();
        let look = (0..buffer.area.width - 4)
            .find(|&x| (0..4).all(|n| buffer[(x + n, row)].symbol() == &"look"[n as usize..][..1]))
            .unwrap();
        // Right after "look ", where the name was.
        let cells = kitty_cells(buffer);
        assert_eq!(cells.len(), 2);
        assert_eq!((cells[0].0, cells[0].1), (look + 5, row));
        assert_eq!((cells[1].0, cells[1].1), (look + 6, row));
        assert!(cells[0].2.starts_with("\x1b_G"));
        assert_eq!(buffer[(look + 7, row)].symbol(), " ");
        assert_eq!(buffer[(look + 8, row)].symbol(), "h");
        // The reaction's picture failed: it keeps its name.
        app.on_matrix(MatrixEvent::EmotePicture {
            url: "mxc://example.org/party".into(),
            result: Err("could not be loaded".into()),
        });
        frame(&mut app, &mut terminal);
        assert!(screen(&terminal).contains(" :?: 3 "));
        assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 2);
        assert!(emote_fetches(&mut commands).is_empty());
    }

    #[test]
    fn a_screen_of_new_custom_emoji_is_fetched_a_few_at_a_time() {
        let names: Vec<String> = (0..20).map(|n| format!("e{n}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut app, mut terminal, mut commands) =
            app_with(kitty(), vec![with_emoji("1", &names)]);
        let first = emote_fetches(&mut commands);
        assert_eq!(first.len(), crate::images::EMOTE_IN_FLIGHT);
        frame(&mut app, &mut terminal);
        assert!(emote_fetches(&mut commands).is_empty());
        // Each answer lets one more go.
        app.on_matrix(MatrixEvent::EmotePicture {
            url: first[0].clone(),
            result: Ok(decoded(64, 64)),
        });
        frame(&mut app, &mut terminal);
        assert_eq!(emote_fetches(&mut commands).len(), 1);

        // Without kitty's protocol, none at all.
        for graphics in [Graphics::halfblocks(), Graphics::off()] {
            let (_app, terminal, mut commands) = app_with(graphics, vec![with_emoji("1", &names)]);
            assert!(emote_fetches(&mut commands).is_empty());
            assert!(screen(&terminal).contains(":e0:"));
        }
    }

    #[test]
    fn a_custom_emoji_arriving_under_a_popup_is_sent_when_the_popup_is_gone() {
        let (mut app, mut terminal, _commands) =
            app_with(kitty(), vec![with_emoji("1", &["blob"])]);
        app.perform(Action::ToggleHelp);
        app.on_matrix(MatrixEvent::EmotePicture {
            url: "mxc://example.org/blob".into(),
            result: Ok(decoded(64, 64)),
        });
        frame(&mut app, &mut terminal);
        // Its cells are kept, but nothing went to the terminal under the popup.
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());
        app.perform(Action::ToggleHelp);
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 2);
        assert!(cells[0].2.starts_with("\x1b_G"));
    }

    const BLOB: &str = "<img data-mx-emoticon src=\"mxc://example.org/blob\" alt=\":blob:\">";

    /// A message whose text is `html`.
    fn formatted(id: &str, html: String) -> Message {
        Message {
            html: Some(html),
            ..text(id, "text")
        }
    }

    fn blob_arrives(app: &mut App) {
        app.on_matrix(MatrixEvent::EmotePicture {
            url: "mxc://example.org/blob".into(),
            result: Ok(decoded(64, 64)),
        });
    }

    /// Cells that carry a picture's data to the terminal.
    fn transmissions(terminal: &Terminal<TestBackend>) -> Vec<(u16, u16)> {
        kitty_cells(terminal.backend().buffer())
            .into_iter()
            .filter(|cell| cell.2.starts_with("\x1b_G"))
            .map(|(x, y, _)| (x, y))
            .collect()
    }

    /// Draws a few more frames with nothing changed, as a clock or a sync would: the
    /// picture neither goes to the terminal again nor is freed there, the cells in
    /// view keep showing it, and no frame asks for another.
    fn stays_put(app: &mut App, terminal: &mut Terminal<TestBackend>, shown: &[(u16, u16)]) {
        for _ in 0..3 {
            app.dirty = false;
            frame(app, terminal);
            assert!(!app.dirty);
            assert!(transmissions(terminal).is_empty());
            assert!(app.images.cleanup_sequence(false).is_empty());
            let cells: Vec<(u16, u16)> = kitty_cells(terminal.backend().buffer())
                .into_iter()
                .map(|(x, y, _)| (x, y))
                .collect();
            assert_eq!(cells, shown);
        }
    }

    /// The popup's rectangle on screen.
    fn completion_popup(app: &App, terminal: &Terminal<TestBackend>) -> ratatui::layout::Rect {
        let area = terminal.backend().buffer().area;
        (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .find_map(|(x, y)| match app.hits.at_rect(x, y) {
                Some((rect, crate::ui::hit::HitId::Overlay)) => Some(rect),
                _ => None,
            })
            .expect("the completion popup is open")
    }

    #[test]
    fn a_custom_emoji_under_the_completion_popup_leaves_its_data_to_a_copy_in_view() {
        // The same emoji twice: on the right of a line, and at the start of the next.
        let (mut app, mut terminal, _commands) = app_with(
            kitty(),
            vec![
                formatted("1", format!("{} {BLOB}", "a".repeat(30))),
                formatted("2", format!("{BLOB} and more")),
            ],
        );
        blob_arrives(&mut app);
        // A mention typed far to the right: the popup opens over the first one.
        app.on_matrix(MatrixEvent::RoomMembers {
            room_id: "!a".into(),
            members: vec![crate::complete::Member {
                user_id: "@alice:example.org".into(),
                display_name: "Alice".into(),
            }],
            can_mention_room: false,
        });
        app.perform(Action::Focus(Pane::Composer));
        for c in format!("{} @al", "b".repeat(28)).chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        app.dirty = false;
        frame(&mut app, &mut terminal);
        assert!(!app.dirty);
        let popup = completion_popup(&app, &terminal);
        let first = (
            row_of(&terminal, "aaaa") as u16,
            row_of(&terminal, "and more") as u16,
        );
        let cells = kitty_cells(terminal.backend().buffer());
        // Only the second one is in view, and it carries the data itself.
        assert_eq!(cells.len(), 2, "{}", screen(&terminal));
        assert_eq!(cells[0].1, first.1);
        assert!(!popup.contains(ratatui::layout::Position::new(cells[0].0, cells[0].1)));
        assert!(popup.y <= first.0 && first.0 < popup.bottom());
        assert_eq!(transmissions(&terminal), [(cells[0].0, cells[0].1)]);
        // Nothing was painted over, nothing is freed, nothing is sent again while
        // the popup stays.
        assert!(app.images.cleanup_sequence(false).is_empty());
        let shown = [(cells[0].0, cells[0].1), (cells[1].0, cells[1].1)];
        stays_put(&mut app, &mut terminal, &shown);

        // Closed: the first one shows too, without being sent again.
        app.on_event(key(KeyCode::Esc));
        app.dirty = false;
        frame(&mut app, &mut terminal);
        assert!(!app.dirty);
        assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 4);
        assert!(transmissions(&terminal).is_empty());
        assert!(app.images.cleanup_sequence(false).is_empty());
    }

    #[test]
    fn someone_newly_ignored_in_place_of_another_shows_no_emoji_pictures() {
        let message = Message {
            sender_id: Some("@alice:example.org".into()),
            ..formatted("1", format!("look {BLOB} here"))
        };
        let (mut app, mut terminal, _commands) = app_with(kitty(), vec![message]);
        app.ignored_users = ["@bob:example.org".to_owned()].into();
        blob_arrives(&mut app);
        frame(&mut app, &mut terminal);
        assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 2);
        // The list changes with its length kept: Bob is not ignored any more, Alice is.
        app.ignored_users = ["@alice:example.org".to_owned()].into();
        frame(&mut app, &mut terminal);
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());
        assert!(screen(&terminal).contains("look :blob: here"));
    }

    #[test]
    fn a_custom_emoji_under_a_corner_pill_leaves_its_data_to_a_copy_in_view() {
        // The same emoji twice in the bottom line: at its start, where the pill of
        // newer messages being loaded goes, and further on.
        let (mut app, mut terminal, _commands) = app_with(
            kitty(),
            vec![formatted("1", format!("{BLOB} {} {BLOB}", "a".repeat(30)))],
        );
        app.timeline.loading_newer = true;
        blob_arrives(&mut app);
        app.dirty = false;
        frame(&mut app, &mut terminal);
        assert!(!app.dirty);
        let row = row_of(&terminal, "aaaa") as u16;
        assert!(screen(&terminal).contains("loading newer messages"));
        assert_eq!(
            row,
            row_of(&terminal, "loading newer messages") as u16,
            "{}",
            screen(&terminal)
        );
        let cells = kitty_cells(terminal.backend().buffer());
        // The first one is under the pill; the second one carries the data.
        assert_eq!(cells.len(), 2, "{}", screen(&terminal));
        assert!(cells.iter().all(|cell| cell.1 == row));
        assert_eq!(transmissions(&terminal), [(cells[0].0, row)]);
        assert!(app.images.cleanup_sequence(false).is_empty());
        let shown = [(cells[0].0, row), (cells[1].0, row)];
        stays_put(&mut app, &mut terminal, &shown);

        // Loaded: the first one shows too, without being sent again.
        app.timeline.loading_newer = false;
        app.dirty = false;
        frame(&mut app, &mut terminal);
        assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 4);
        assert!(transmissions(&terminal).is_empty());
        assert!(app.images.cleanup_sequence(false).is_empty());
    }

    #[test]
    fn a_picture_whose_corner_is_under_the_completion_popup_waits_for_it_to_close() {
        // Five rows high, at the bottom of the view; its data travels in its top left
        // corner.
        let (mut app, mut terminal, _commands) = app_with(
            kitty(),
            vec![picture("1", remote_image(KEY, Some((320, 100))))],
        );
        app.on_matrix(loaded(320, 100));
        frame(&mut app, &mut terminal);
        let corner = transmissions(&terminal);
        assert_eq!(corner.len(), 1);
        let corner = corner[0];
        // The terminal forgets it (as after a resize), and a shortcode at the start of
        // the draft opens the popup over that corner.
        app.images.forget_encoded();
        app.images.cleanup_sequence(false);
        app.perform(Action::Focus(Pane::Composer));
        for c in ":sm".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        frame(&mut app, &mut terminal);
        let popup = completion_popup(&app, &terminal);
        assert!(
            popup.contains(ratatui::layout::Position::new(corner.0, corner.1)),
            "{}",
            screen(&terminal)
        );
        // Nothing went to the terminal to be painted over, so nothing has to be freed
        // or sent again.
        for _ in 0..2 {
            assert!(transmissions(&terminal).is_empty());
            assert!(app.images.cleanup_sequence(false).is_empty());
            frame(&mut app, &mut terminal);
        }

        app.on_event(key(KeyCode::Esc));
        frame(&mut app, &mut terminal);
        assert_eq!(transmissions(&terminal), [corner]);
        assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 32 * 5);
    }

    /// An app connected to a fake worker with `count` rooms, each with a picture.
    fn app_with_rooms(
        graphics: Graphics,
        count: usize,
    ) -> (App, Terminal<TestBackend>, UnboundedReceiver<MatrixCmd>) {
        let (mut app, mut terminal, commands) = app_with(graphics, Vec::new());
        let rooms = (0..count)
            .map(|n| RoomSummary {
                encrypted: false,
                id: format!("!r{n}"),
                name: format!("Room {n:02}"),
                category: RoomCategory::Room,
                notifications: 0,
                server_notifications: 0,
                mention: false,
                unread: false,
                marked_unread: false,
                preview: None,
                avatar: Some(format!("mxc://example.org/avatar{n}")),
                alias: None,
                direct: false,
                muted: false,
            })
            .collect();
        app.on_matrix(MatrixEvent::Rooms(rooms));
        frame(&mut app, &mut terminal);
        (app, terminal, commands)
    }

    fn avatar_fetches(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<String> {
        let mut seen = Vec::new();
        while let Ok(command) = commands.try_recv() {
            if let MatrixCmd::FetchAvatar { url } = command {
                seen.push(url);
            }
        }
        seen
    }

    fn avatar(n: usize, ok: bool) -> MatrixEvent {
        MatrixEvent::Avatar {
            url: format!("mxc://example.org/avatar{n}"),
            result: if ok {
                Ok(decoded(96, 96))
            } else {
                Err("could not be loaded".into())
            },
        }
    }

    #[test]
    fn a_room_cut_by_the_edge_of_the_list_shows_half_its_picture() {
        let (mut app, mut terminal, _commands) = app_with_rooms(kitty(), 40);
        app.room_previews = true;
        frame(&mut app, &mut terminal);
        for n in 0..40 {
            app.on_matrix(avatar(n, true));
        }
        // Rows of picture cells per screen line, top to bottom.
        let lines = |terminal: &Terminal<TestBackend>| -> Vec<(u16, usize)> {
            let mut per_line: std::collections::BTreeMap<u16, usize> = Default::default();
            for (_, y, _) in kitty_cells(terminal.backend().buffer()) {
                *per_line.entry(y).or_default() += 1;
            }
            per_line.into_iter().collect()
        };
        // Whole rooms: every picture line has the four cells of one picture, and
        // lines come in pairs. Then scroll by a single line, so that the first room
        // loses its upper line and the last one its lower line.
        app.sidebar.scroll = 1;
        frame(&mut app, &mut terminal);
        let even = lines(&terminal);
        app.sidebar.scroll = 2;
        frame(&mut app, &mut terminal);
        let odd = lines(&terminal);
        for shown in [&even, &odd] {
            assert!(shown.iter().all(|(_, cells)| *cells == 4), "{shown:?}");
        }
        // Every line of the list below the heading carries a picture in both cases:
        // a half picture at an edge, not a blank or a bare tile.
        let list_lines = |shown: &Vec<(u16, usize)>| shown.len();
        assert_eq!(list_lines(&even), list_lines(&odd));
        let first_even = even.first().unwrap().0;
        let first_odd = odd.first().unwrap().0;
        assert_eq!(
            first_even, first_odd,
            "the top line has a picture either way"
        );
    }

    #[test]
    fn room_pictures_replace_the_tile_for_visible_rows_only() {
        let (mut app, mut terminal, mut commands) = app_with_rooms(kitty(), 60);
        // Asked for once, and only for the rooms on screen: 25 rows of list below
        // the filter line and quick-filter chips, one of them the section title.
        let wanted = avatar_fetches(&mut commands);
        assert_eq!(wanted.len(), 24, "{wanted:?}");
        assert!(wanted.contains(&"mxc://example.org/avatar0".to_owned()));
        assert!(!wanted.contains(&"mxc://example.org/avatar40".to_owned()));
        frame(&mut app, &mut terminal);
        assert!(avatar_fetches(&mut commands).is_empty());

        // Until it is there, the tile with the initial.
        let row = row_of(&terminal, "Room 03") as u16;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, row)].symbol(), "R");
        assert!(kitty_cells(buffer).is_empty());

        app.on_matrix(avatar(3, true));
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        // Exactly the tile's two cells, the picture itself travelling in the first.
        assert_eq!(cells.len(), 2);
        assert_eq!((cells[0].0, cells[0].1), (2, row));
        assert_eq!((cells[1].0, cells[1].1), (3, row));
        assert!(cells[0].2.starts_with("\x1b_G"));
        // The row is otherwise as before, and the others keep their tiles.
        assert!(screen(&terminal).contains("Room 03"));
        assert_eq!(terminal.backend().buffer()[(2, row + 1)].symbol(), "R");
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 2);
        assert!(!cells[0].2.contains('\x1b'));

        // A failure keeps the tile and says nothing.
        app.on_matrix(avatar(4, false));
        frame(&mut app, &mut terminal);
        assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 2);
        assert!(!screen(&terminal).contains("could not"));
        assert_eq!(terminal.backend().buffer()[(2, row + 1)].symbol(), "R");
        assert!(avatar_fetches(&mut commands).is_empty());

        // Scrolled: the picture moves with its row or leaves with it, and the rows
        // that come into view are asked for.
        app.sidebar.scroll = 2;
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!((cells.len(), cells[0].1), (2, row - 2));
        assert_eq!(avatar_fetches(&mut commands).len(), 2);
        app.sidebar.scroll = 30;
        frame(&mut app, &mut terminal);
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());

        // Filtered down to that room, it is shown again without being sent again.
        app.sidebar.scroll = 0;
        app.sidebar.filter = "Room 03".into();
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 2);
        assert!(!cells[0].2.contains('\x1b'));
        app.sidebar.filter.clear();
        avatar_fetches(&mut commands);

        // The room list hidden in a narrow window: nothing of it stays.
        terminal.backend_mut().resize(50, 20);
        app.on_event(InputEvent::Resize(50, 20));
        frame(&mut app, &mut terminal);
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());
        // Back: after a resize it is sent to the terminal again.
        terminal.backend_mut().resize(80, 30);
        app.on_event(InputEvent::Resize(80, 30));
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 2);
        assert!(cells[0].2.starts_with("\x1b_G"));
        app.images.cleanup_sequence(false);

        // Logging out forgets them and has the terminal free its copy.
        app.on_matrix(MatrixEvent::NeedLogin { reason: None });
        assert_eq!(
            app.images
                .cleanup_sequence(false)
                .matches("a=d,d=I")
                .count(),
            1
        );
        // One that was still on its way is ignored.
        app.on_matrix(avatar(5, true));
        assert!(app.images.take_avatar_requests().is_empty());
    }

    #[test]
    fn a_room_picture_arriving_under_a_popup_waits_for_it_to_close() {
        let (mut app, mut terminal, _commands) = app_with_rooms(kitty(), 3);
        app.perform(Action::ToggleHelp);
        app.on_matrix(avatar(0, true));
        frame(&mut app, &mut terminal);
        assert!(kitty_cells(terminal.backend().buffer()).is_empty());
        app.perform(Action::ToggleHelp);
        frame(&mut app, &mut terminal);
        let cells = kitty_cells(terminal.backend().buffer());
        assert_eq!(cells.len(), 2);
        assert!(cells[0].2.starts_with("\x1b_G"));
        // The highlight of the open room is still there around the picture.
        let row = cells[0].1;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, row)].bg, buffer[(8, row)].bg);
        assert_eq!(
            buffer[(1, row)].bg,
            app.theme.style(crate::theme::Role::RoomActive).bg.unwrap()
        );
    }

    #[test]
    fn two_rooms_sharing_a_picture_send_it_once() {
        let (mut app, mut terminal, _commands) = app_with_rooms(kitty(), 3);
        app.rooms[1].avatar = app.rooms[0].avatar.clone();
        app.on_matrix(avatar(0, true));
        frame(&mut app, &mut terminal);
        let transmissions = |terminal: &Terminal<TestBackend>| {
            kitty_cells(terminal.backend().buffer())
                .iter()
                .filter(|cell| cell.2.starts_with("\x1b_G"))
                .count()
        };
        // Both rooms show it (4 cells), the data travels in one of them.
        assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 4);
        assert_eq!(transmissions(&terminal), 1);
        // And it stays sent: later frames neither delete nor repeat it.
        for _ in 0..3 {
            app.dirty = true;
            frame(&mut app, &mut terminal);
            assert_eq!(kitty_cells(terminal.backend().buffer()).len(), 4);
            assert_eq!(transmissions(&terminal), 0);
            assert!(app.images.cleanup_sequence(false).is_empty());
        }
    }

    #[test]
    fn without_real_graphics_rooms_keep_their_tiles() {
        for graphics in [Graphics::halfblocks(), Graphics::off()] {
            let (mut app, mut terminal, mut commands) = app_with_rooms(graphics, 5);
            assert!(avatar_fetches(&mut commands).is_empty());
            app.on_matrix(avatar(0, true));
            frame(&mut app, &mut terminal);
            let row = row_of(&terminal, "Room 01") as u16;
            assert_eq!(terminal.backend().buffer()[(2, row)].symbol(), "R");
            assert!(picture_cells(terminal.backend().buffer()).is_empty());
        }
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
