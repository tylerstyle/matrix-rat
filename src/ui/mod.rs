//! Drawing. Each frame also rebuilds the hit map used for mouse handling.

pub(crate) mod attach;
mod audio;
mod collect;
mod complete;
mod composer;
mod details;
mod devices;
mod doctor;
mod global_search;
mod help;
pub mod hit;
mod image;
mod keybinds;
mod keywords;
mod menu;
mod modal;
mod nochat;
mod palette;
mod people;
mod poll;
mod roominfo;
mod rooms_bar;
mod saved;
mod search;
mod sidebar;
mod status;
mod stickers;
mod threads;
mod timeline;
mod verify;

pub use composer::Mic;
#[cfg(test)]
pub use composer::SYMBOLS;
pub use timeline::RowCache;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType};

use crate::action::Pane;
use crate::app::App;
use crate::theme::Role;
use hit::HitId;

/// Below this width the room list and the chat are shown one at a time.
pub const NARROW_WIDTH: u16 = 60;

/// Whether one pane fills the window: it is narrow, or the user put the room list away.
pub fn one_pane(app: &App, width: u16) -> bool {
    width < NARROW_WIDTH || app.sidebar.hidden
}
pub const SIDEBAR_MIN: u16 = 16;

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits.clear();
    app.images.begin_frame();
    // The composer says where it is when it is drawn. In a frame without it (the room
    // list alone, or too little room) what it said for an earlier, perhaps larger,
    // window must not place the emoji picker or a menu.
    app.composer_view.area = Rect::default();
    app.composer_view.cursor = None;
    let area = frame.area();
    app.screen = area;
    frame
        .buffer_mut()
        .set_style(area, app.theme.style(Role::Text));

    let [main, status_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

    let narrow = one_pane(app, area.width);
    let (sidebar_area, chat_area) = if narrow {
        // With no chat open the room list is the one pane there is.
        if app.focus == Pane::Sidebar || app.chat_closed() {
            (Some(main), None)
        } else {
            (None, Some(main))
        }
    } else {
        let width = app
            .sidebar
            .width
            .clamp(SIDEBAR_MIN, max_sidebar(area.width));
        let [side, chat] =
            Layout::horizontal([Constraint::Length(width), Constraint::Fill(1)]).areas(main);
        (Some(side), Some(chat))
    };

    if let Some(area) = sidebar_area {
        sidebar::draw(frame, app, area);
        if !narrow {
            // The room list's right border doubles as the resize handle.
            let handle = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
            app.hits.add(handle, HitId::Splitter);
        }
    }
    if let Some(area) = chat_area
        && app.chat_closed()
    {
        nochat::draw(frame, app, area);
    } else if let Some(area) = chat_area {
        // With the room list out of sight, the first line is the way back to it.
        let bar = rooms_bar::height(app, narrow, area);
        let [bar_area, area] =
            Layout::vertical([Constraint::Length(bar), Constraint::Fill(1)]).areas(area);
        if bar > 0 {
            rooms_bar::draw(frame, app, bar_area);
        }
        let composer_height = composer::height(app, area.width);
        let [timeline_area, composer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(composer_height)])
                .areas(area);
        // The composer first: where its cursor is decides where the completion popup
        // goes over the timeline, and the timeline's pictures keep clear of it.
        composer::draw(frame, app, composer_area);
        let popup = complete::area(app, area, frame.area(), composer_area.y);
        timeline::draw(frame, app, timeline_area, popup);
        complete::draw(frame, app, area, composer_area.y);
    }
    status::draw(frame, app, status_area);
    image::draw_viewer(frame, app, area);
    if app.help_open {
        help::draw(frame, app, area);
    }
    modal::draw(frame, app, area);
    image::verify(frame, app);
}

pub fn max_sidebar(screen_width: u16) -> u16 {
    (screen_width / 2).max(SIDEBAR_MIN)
}

/// The bordered frame of a pane. The digit in the title is its `Alt-<n>` shortcut.
fn pane_block(app: &App, pane: Pane, title: &str) -> Block<'static> {
    let focused = app.focus == pane;
    let (border, title_style) = if focused {
        (Role::BorderFocused, Role::TitleFocused)
    } else {
        (Role::Border, Role::Title)
    };
    let digit = match pane {
        Pane::Sidebar => 1,
        Pane::Timeline => 2,
        Pane::Composer => 3,
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(app.theme.style(border))
        .title(Line::from(Span::styled(
            format!(" {digit} {title} "),
            app.theme.style(title_style),
        )))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::action::{Action, Pane};
    use crate::app::App;

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| super::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    /// The screen as an SVG picture, for the README: every cell with its colours, so
    /// it looks like the terminal without needing one.
    fn svg(app: &mut App, width: u16, height: u16) -> String {
        use ratatui::style::{Color, Modifier};
        use std::fmt::Write as _;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| super::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        const CELL_W: f32 = 8.4;
        const CELL_H: f32 = 18.0;
        let hex = |color: Color, base: &str| match color {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => base.to_owned(),
        };
        // The theme's own background and text colour stand in wherever a cell
        // leaves them unset.
        let text = app.theme.style(crate::theme::Role::Text);
        let base_bg = hex(text.bg.unwrap_or(Color::Reset), "#16161d");
        let base_fg = hex(text.fg.unwrap_or(Color::Reset), "#dcd7ba");
        let (base_bg, base_fg) = (base_bg.as_str(), base_fg.as_str());
        let (w, h) = (f32::from(width) * CELL_W, f32::from(height) * CELL_H);
        let mut out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w:.0} {h:.0}\" \
             font-family=\"ui-monospace, 'JetBrains Mono', 'DejaVu Sans Mono', Menlo, Consolas, monospace\" \
             font-size=\"14\">\n<clipPath id=\"screen\"><rect width=\"{w:.0}\" height=\"{h:.0}\" rx=\"8\"/></clipPath>\n\
             <g clip-path=\"url(#screen)\">\n<rect width=\"100%\" height=\"100%\" fill=\"{base_bg}\"/>\n"
        );
        for y in 0..height {
            // Backgrounds first, as runs of equal colour.
            let mut x = 0;
            while x < width {
                let cell = &buffer[(x, y)];
                let reversed = cell.modifier.contains(Modifier::REVERSED);
                let bg = if reversed {
                    hex(cell.fg, base_fg)
                } else {
                    hex(cell.bg, base_bg)
                };
                let start = x;
                while x < width && {
                    let c = &buffer[(x, y)];
                    let r = c.modifier.contains(Modifier::REVERSED);
                    (if r {
                        hex(c.fg, base_fg)
                    } else {
                        hex(c.bg, base_bg)
                    }) == bg
                } {
                    x += 1;
                }
                if bg != base_bg {
                    let _ = writeln!(
                        out,
                        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{CELL_H}\" fill=\"{bg}\"/>",
                        f32::from(start) * CELL_W,
                        f32::from(y) * CELL_H,
                        f32::from(x - start) * CELL_W + 0.2,
                    );
                }
            }
            // Then the text, as runs of equal style; a run is stretched to exactly its
            // cells, so columns line up whatever font the viewer has.
            let mut x = 0;
            while x < width {
                let cell = &buffer[(x, y)];
                let style = (cell.fg, cell.bg, cell.modifier);
                let start = x;
                let mut text = String::new();
                let mut cells = 0u16;
                while x < width {
                    let c = &buffer[(x, y)];
                    if (c.fg, c.bg, c.modifier) != style {
                        break;
                    }
                    let symbol = c.symbol();
                    // Lines and blocks are drawn, not typed: fonts differ too much.
                    if let Some(drawn) = shape(symbol, f32::from(x) * CELL_W, f32::from(y) * CELL_H)
                    {
                        if !text.is_empty() {
                            break;
                        }
                        let reversed = c.modifier.contains(Modifier::REVERSED);
                        let fg = if reversed {
                            hex(c.bg, base_bg)
                        } else {
                            hex(c.fg, base_fg)
                        };
                        let opacity = if c.modifier.contains(Modifier::DIM) {
                            " opacity=\"0.55\""
                        } else {
                            ""
                        };
                        let _ =
                            writeln!(out, "<g fill=\"{fg}\" stroke=\"{fg}\"{opacity}>{drawn}</g>");
                        x += 1;
                        cells = 0;
                        break;
                    }
                    let wide = crate::text::width(symbol) > 1;
                    // A wide character stands alone, so it cannot shift its neighbours.
                    if wide && !text.is_empty() {
                        break;
                    }
                    text.push_str(symbol);
                    let step = crate::text::width(symbol).max(1) as u16;
                    cells += step;
                    x += step;
                    if wide {
                        break;
                    }
                }
                if text.trim().is_empty() {
                    continue;
                }
                let reversed = style.2.contains(Modifier::REVERSED);
                let fg = if reversed {
                    hex(style.1, base_bg)
                } else {
                    hex(style.0, base_fg)
                };
                let escaped = text
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;");
                let weight = if style.2.contains(Modifier::BOLD) {
                    " font-weight=\"bold\""
                } else {
                    ""
                };
                let slant = if style.2.contains(Modifier::ITALIC) {
                    " font-style=\"italic\""
                } else {
                    ""
                };
                let opacity = if style.2.contains(Modifier::DIM) {
                    " opacity=\"0.55\""
                } else {
                    ""
                };
                let decoration = if style.2.contains(Modifier::CROSSED_OUT) {
                    " text-decoration=\"line-through\""
                } else if style.2.contains(Modifier::UNDERLINED) {
                    " text-decoration=\"underline\""
                } else {
                    ""
                };
                let _ = writeln!(
                    out,
                    "<text x=\"{:.1}\" y=\"{:.1}\" textLength=\"{:.1}\" lengthAdjust=\"spacingAndGlyphs\" \
                     xml:space=\"preserve\" fill=\"{fg}\"{weight}{slant}{opacity}{decoration}>{escaped}</text>",
                    f32::from(start) * CELL_W,
                    f32::from(y) * CELL_H + 13.5,
                    f32::from(cells) * CELL_W,
                );
            }
        }
        out.push_str("</g>\n</svg>\n");
        out
    }

    /// Box-drawing and block characters as SVG shapes within their cell.
    fn shape(symbol: &str, x: f32, y: f32) -> Option<String> {
        const W: f32 = 8.4;
        const H: f32 = 18.0;
        let (cx, cy) = (x + W / 2.0, y + H / 2.0);
        let (l, r, t, b) = (x - 0.1, x + W + 0.1, y - 0.1, y + H + 0.1);
        let line = |x1: f32, y1: f32, x2: f32, y2: f32, width: f32| {
            format!(
                "<path d=\"M{x1:.1} {y1:.1} L{x2:.1} {y2:.1}\" stroke-width=\"{width}\" fill=\"none\"/>"
            )
        };
        let rect = |x: f32, y: f32, w: f32, h: f32| {
            format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" stroke=\"none\"/>"
            )
        };
        // A rounded corner: from one edge's middle to the centre region and on.
        let corner = |from: (f32, f32), to: (f32, f32)| {
            format!(
                "<path d=\"M{:.1} {:.1} Q{cx:.1} {cy:.1} {:.1} {:.1}\" stroke-width=\"1\" fill=\"none\"/>",
                from.0, from.1, to.0, to.1
            )
        };
        Some(match symbol {
            "─" => line(l, cy, r, cy, 1.0),
            "│" => line(cx, t, cx, b, 1.0),
            "┃" => line(cx, t, cx, b, 2.2),
            "╭" | "┌" => corner((cx, b), (r, cy)),
            "╮" | "┐" => corner((l, cy), (cx, b)),
            "╰" | "└" => corner((cx, t), (r, cy)),
            "╯" | "┘" => corner((l, cy), (cx, t)),
            "├" => format!("{}{}", line(cx, t, cx, b, 1.0), line(cx, cy, r, cy, 1.0)),
            "┤" => format!("{}{}", line(cx, t, cx, b, 1.0), line(l, cy, cx, cy, 1.0)),
            "▌" => rect(x, y, W / 2.0, H),
            "▐" => rect(x + W / 2.0, y, W / 2.0, H),
            "▀" => rect(x, y, W + 0.2, H / 2.0),
            "▄" => rect(x, y + H / 2.0, W + 0.2, H / 2.0),
            "█" => rect(x, y, W + 0.2, H),
            _ => return None,
        })
    }

    /// Writes the pictures the README shows. Run after changing the look:
    /// `cargo test write_screenshots -- --ignored`
    #[test]
    #[ignore = "writes docs/screenshots"]
    fn write_screenshots() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots");
        std::fs::create_dir_all(&dir).unwrap();
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));

        // The main view, with room previews and a picture.
        let mut app = App::demo();
        app.room_previews = true;
        let design = app.rooms.iter().position(|r| r.name == "design").unwrap();
        render(&mut app, 110, 34);
        app.perform(Action::OpenRoom(design));
        std::fs::write(dir.join("chat.svg"), svg(&mut app, 110, 34)).unwrap();

        // The command palette.
        let mut app = App::demo();
        app.room_previews = true;
        render(&mut app, 110, 34);
        app.perform(Action::OpenPalette);
        for c in "th".chars() {
            app.on_event(key(KeyCode::Char(c)));
        }
        std::fs::write(dir.join("palette.svg"), svg(&mut app, 110, 34)).unwrap();

        // A thread, in another theme.
        let mut app = App::demo();
        app.room_previews = true;
        app.perform(Action::CycleTheme);
        let rat = app
            .rooms
            .iter()
            .position(|r| r.name == "matrix-rat")
            .unwrap();
        render(&mut app, 110, 34);
        app.perform(Action::OpenRoom(rat));
        render(&mut app, 110, 34);
        // Up selects the newest message; walk up to the one that started a thread.
        for _ in 0..12 {
            app.on_event(key(KeyCode::Up));
            let selected = app
                .timeline
                .selected
                .and_then(|i| app.rooms[rat].messages.get(i));
            if selected.is_some_and(|m| m.thread.is_some()) {
                break;
            }
        }
        app.on_event(key(KeyCode::Char('t')));
        assert!(app.threads.open.is_some(), "the demo has a thread to show");
        std::fs::write(dir.join("theme.svg"), svg(&mut app, 110, 34)).unwrap();

        // A narrow window: going to the room list shows it in place of the chat, and
        // opening a room puts the list away again, with the rooms bar above the chat.
        let (narrow, tall) = (NARROW_SHOT_WIDTH, 30);
        let mut app = App::demo();
        app.room_previews = true;
        render(&mut app, narrow, tall);
        let open = app.active;
        app.perform(Action::Focus(Pane::Sidebar));
        let list = render(&mut app, narrow, tall);
        assert!(!list.contains("☰ Rooms"), "the room list fills the window");
        std::fs::write(dir.join("narrow-rooms.svg"), svg(&mut app, narrow, tall)).unwrap();
        app.perform(Action::OpenRoom(open));
        let chat = render(&mut app, narrow, tall);
        assert!(
            chat.contains("☰ Rooms"),
            "the rooms bar shows above the chat"
        );
        std::fs::write(dir.join("narrow-chat.svg"), svg(&mut app, narrow, tall)).unwrap();
    }

    /// Narrow enough that one pane fills the window (below [`super::NARROW_WIDTH`]).
    const NARROW_SHOT_WIDTH: u16 = 56;
    const _: () = assert!(NARROW_SHOT_WIDTH < super::NARROW_WIDTH);

    #[test]
    fn main_screen() {
        let mut app = App::demo();
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn message_selected_and_reply() {
        let mut app = App::demo();
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        app.perform(Action::Reply);
        app.composer.insert("Nice, congrats!\nSecond line");
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn narrow_shows_one_pane() {
        let mut app = App::demo();
        insta::assert_snapshot!("narrow_chat", render(&mut app, 50, 20));
        app.perform(Action::Focus(Pane::Sidebar));
        insta::assert_snapshot!("narrow_rooms", render(&mut app, 50, 20));
    }

    /// The demo with no chat open, as after Esc in the composer.
    fn closed_app() -> App {
        let mut app = App::demo();
        render(&mut app, 100, 30);
        app.perform(Action::Back);
        assert!(app.chat_closed());
        app
    }

    #[test]
    fn no_chat_open() {
        let mut app = closed_app();
        insta::assert_snapshot!("no_chat_open", render(&mut app, 100, 30));
    }

    #[test]
    fn no_chat_open_at_80x24() {
        let mut app = closed_app();
        insta::assert_snapshot!("no_chat_open_80x24", render(&mut app, 80, 24));
    }

    #[test]
    fn no_chat_open_in_a_narrow_window() {
        // One pane at a time: the room list is the one there is.
        let mut app = closed_app();
        insta::assert_snapshot!("no_chat_open_narrow", render(&mut app, 50, 20));
    }

    #[test]
    fn no_chat_open_in_a_low_or_tiny_window() {
        let mut app = closed_app();
        // Too low for the name: the keys alone, as many as fit.
        let low = render(&mut app, 100, 6);
        assert!(low.contains("open the selected room"), "{low}");
        assert!(!low.contains("all keys"), "{low}");
        // Nothing breaks in a window too small for anything.
        for (width, height) in [(61, 3), (60, 2), (64, 4), (20, 5), (1, 1)] {
            render(&mut app, width, height);
        }
    }

    #[test]
    fn no_chat_open_names_the_keys_as_they_are_bound() {
        let mut app = closed_app();
        let screen = render(&mut app, 100, 30);
        for line in [
            " Enter  open the selected room",
            "Ctrl-K  find a room",
            "Ctrl-P  command palette",
            "    F1  all keys",
        ] {
            assert!(screen.contains(line), "{line:?} not in:\n{screen}");
        }
        // Other keys, and none at all for the help: its line is not shown.
        let config = crate::config::Config::parse(
            "[keys.global]\npalette = \"f9\"\nhelp = \"none\"\n\
             [keys.sidebar]\nsidebar_open = \"o\"\nhelp = \"none\"\n",
        )
        .unwrap();
        app.keymap = crate::keymap::Keymap::from_config(&config).0;
        let screen = render(&mut app, 100, 30);
        assert!(
            screen.contains("     o  open the selected room"),
            "{screen}"
        );
        assert!(screen.contains("    F9  command palette"), "{screen}");
        assert!(!screen.contains("all keys"), "{screen}");
    }

    #[test]
    fn the_room_list_can_be_put_away_in_a_wide_window() {
        let mut app = App::demo();
        let wide = render(&mut app, 100, 20);
        assert!(wide.contains("1 Rooms") && wide.contains("2 #"));
        // Put away: the chat has the whole width.
        app.perform(Action::ToggleSidebar);
        let chat = render(&mut app, 100, 20);
        assert!(!chat.contains("1 Rooms") && chat.contains("2 #"), "{chat}");
        // Going to the list shows it in the chat's place; opening a room brings the
        // chat back.
        app.perform(Action::Focus(Pane::Sidebar));
        let rooms = render(&mut app, 100, 20);
        assert!(
            rooms.contains("1 Rooms") && !rooms.contains("2 #"),
            "{rooms}"
        );
        app.perform(Action::Focus(Pane::Composer));
        assert!(!render(&mut app, 100, 20).contains("1 Rooms"));
        // Put away while the list has the focus: the focus goes to the composer.
        app.perform(Action::ToggleSidebar);
        app.perform(Action::Focus(Pane::Sidebar));
        app.perform(Action::ToggleSidebar);
        assert_eq!(app.focus, Pane::Composer);
        app.perform(Action::ToggleSidebar);
        assert!(render(&mut app, 100, 20).contains("1 Rooms"));
    }

    #[test]
    fn help_overlay() {
        let mut app = App::demo();
        app.perform(Action::ToggleHelp);
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn room_filter() {
        let mut app = App::demo();
        app.perform(Action::FocusRoomFilter);
        for c in "ma".chars() {
            app.type_char(c);
        }
        insta::assert_snapshot!(render(&mut app, 100, 16));
    }

    #[test]
    fn settings_menu() {
        let mut app = App::demo();
        render(&mut app, 100, 30);
        app.perform(Action::OpenSettings);
        insta::assert_snapshot!(render(&mut app, 100, 30));
        // A low window drops the blank lines but keeps every row.
        insta::assert_snapshot!("settings_menu_low", render(&mut app, 70, 18));
    }

    #[test]
    fn settings_on_a_small_terminal_keep_the_hint_and_say_what_is_out_of_sight() {
        use crate::modal::Modal;
        let mut app = App::demo();
        render(&mut app, 80, 24);
        app.perform(Action::OpenSettings);
        // With the dark and light slots shown there are more rows than lines.
        if let Some(Modal::Settings(menu)) = &mut app.modal {
            menu.show_theme_pair = true;
        }
        let top = render(&mut app, 80, 24);
        // The hint of the selected row is there, and a mark for rows further down.
        assert!(top.contains("▼"), "{top}");
        assert!(!top.contains("▲"), "{top}");
        let hint = match &app.modal {
            Some(Modal::Settings(menu)) => menu.current().hint(),
            _ => panic!("no settings"),
        };
        let first_words: String = hint
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            top.contains(&first_words),
            "hint missing: {first_words}\n{top}"
        );
        insta::assert_snapshot!("settings_small_top", top);
        // At the last row: Log out is in view, the mark points up, and asking to
        // confirm fits its column.
        if let Some(Modal::Settings(menu)) = &mut app.modal {
            menu.step(-1);
            menu.confirm_logout = true;
        }
        let bottom = render(&mut app, 80, 24);
        assert!(bottom.contains("▲") && !bottom.contains("▼"), "{bottom}");
        assert!(
            bottom.contains("Enter again logs out · Esc stays"),
            "{bottom}"
        );
        insta::assert_snapshot!("settings_small_bottom", bottom);
    }

    /// Every hint the settings can show is whole in the two lines it has, as ratatui
    /// wraps it in the box at its full width: down to its last words, which are often
    /// what leaves the machine or how a value is cleared.
    #[test]
    fn every_settings_hint_is_whole_in_its_two_lines() {
        use crate::modal::{Modal, Setting, SettingsMenu};
        // Room for the box at its full width and every row: the hint gets its two
        // lines.
        let (width, height) = (100, 70);
        let mut checked = 0;
        for show_theme_pair in [false, true] {
            for setting in Setting::ALL {
                let menu = SettingsMenu::select(setting, show_theme_pair);
                if !menu.shows(setting) {
                    continue;
                }
                let mut app = App::demo();
                app.modal = Some(Modal::Settings(menu));
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| super::draw(frame, &mut app)).unwrap();
                let buffer = terminal.backend().buffer();
                let text = |y: u16, xs: std::ops::Range<u16>| -> String {
                    xs.map(|x| buffer[(x, y)].symbol()).collect()
                };
                // The box: its top border carries the title.
                let (top, left) = (0..height)
                    .find_map(|y| {
                        (0..width - 10)
                            .find(|&x| text(y, x..x + 10) == "╭ Settings")
                            .map(|x| (y, x))
                    })
                    .expect("the settings box");
                let right = (left + 1..width)
                    .find(|&x| buffer[(x, top)].symbol() == "╮")
                    .expect("its right corner");
                let bottom = (top + 1..height)
                    .find(|&y| buffer[(left, y)].symbol() == "╰")
                    .expect("its bottom");
                assert_eq!(right - left + 1, 62, "the box at its full width");
                // What is inside, row by row; the hint is at the end.
                let inside = (top + 1..bottom)
                    .map(|y| text(y, left + 1..right).trim().to_owned())
                    .filter(|row| !row.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                let hint = setting.hint();
                assert!(
                    inside.ends_with(hint),
                    "{setting:?}: {hint}\n{}",
                    render(&mut app, width, height)
                );
                checked += 1;
            }
        }
        assert_eq!(checked, Setting::ALL.len() * 2 - 2);
        assert!(Setting::SummaryCommand.hint().contains("(/catchup N)"));
    }

    #[test]
    fn room_previews() {
        let mut app = App::demo();
        app.room_previews = true;
        app.rooms[2].preview = Some("bob: are you coming tonight? we start at eight".into());
        insta::assert_snapshot!(render(&mut app, 100, 30));
        // Narrow and low windows cut the lines but keep the pairs together.
        for (width, height) in [(40, 9), (24, 6), (12, 4)] {
            render(&mut app, width, height);
        }
    }

    #[test]
    fn keybinds_editor_and_vim_help() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::demo();
        app.perform(Action::OpenSettings);
        // Move to Setting::Keys and press Enter to open the key-bindings editor.
        app.perform(Action::Setting(crate::modal::Setting::Keys));
        insta::assert_snapshot!("keybinds_editor", render(&mut app, 100, 30));

        // Select "Next pane" (Tab) -> Capture -> press Ctrl-Q -> no, press Alt-E -> conflict prompt.
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::ALT,
        )));
        insta::assert_snapshot!("keybinds_conflict", render(&mut app, 100, 30));

        // Switch to vim preset, close editor, open F1 help overlay to inspect rendered vim bindings.
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_event(Event::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        app.on_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.perform(Action::ToggleHelp);
        insta::assert_snapshot!("keybinds_vim_help", render(&mut app, 100, 30));
    }

    #[test]
    fn detached_banner() {
        let mut app = App::demo();
        render(&mut app, 100, 30);
        app.timeline.detached = true;
        app.timeline.reached_end = false;
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn find_bar() {
        let mut app = App::demo();
        render(&mut app, 100, 30);
        app.perform(Action::Search);
        for c in "cache".chars() {
            app.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(c),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        insta::assert_snapshot!(render(&mut app, 100, 30));
        // Small windows: it shrinks, and below a minimum it is simply not drawn.
        for (width, height) in [(30, 8), (17, 5), (10, 3)] {
            render(&mut app, width, height);
        }
    }

    #[test]
    fn emoji_picker() {
        let mut app = App::demo();
        render(&mut app, 100, 30);
        app.perform(Action::OpenEmoji);
        for c in "cat".chars() {
            app.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(c),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn emoji_picker_custom() {
        use crate::matrix::{CustomEmoji, EmoteCatalog, EmotePack};
        let mut app = App::demo();
        let emoji = |code: &str| CustomEmoji {
            shortcode: code.into(),
            url: format!("mxc://example.org/{code}"),
            body: None,
            pack_name: "Rat pack".into(),
        };
        app.emotes.catalog = EmoteCatalog {
            packs: vec![EmotePack {
                id: "user".into(),
                name: "Rat pack".into(),
                emojis: ["ratwave", "ratparty", "blobrat", "a_very_long_rat_name"]
                    .map(emoji)
                    .to_vec(),
                stickers: Vec::new(),
            }],
        };
        // On an 80x24 terminal, reacting to a message: the quick reactions, then the
        // custom emoji by name, then the rest.
        render(&mut app, 80, 24);
        app.timeline.selected = Some(1);
        app.perform(Action::React);
        // Drawn once, so that the arrows know the grid: down to the second row of
        // custom emoji.
        render(&mut app, 80, 24);
        for _ in 0..2 {
            app.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn message_kinds() {
        use crate::model::{Message, MsgKind, SendState};
        let mut app = App::demo();
        let base = |sender: &str, body: &str| Message {
            sender: sender.into(),
            time: "12:00".into(),
            day: "Today".into(),
            body: body.into(),
            ..Message::default()
        };
        let room = &mut app.rooms[0];
        room.typing = None;
        room.messages = vec![
            Message {
                kind: MsgKind::System,
                ..base("", "alice created the room")
            },
            base("alice", "A normal message."),
            Message {
                kind: MsgKind::Emote,
                ..base("bob", "waves")
            },
            Message {
                kind: MsgKind::Redacted,
                ..base("bob", "Message deleted")
            },
            Message {
                kind: MsgKind::Undecryptable,
                ..base("bob", "Unable to decrypt (waiting for keys)")
            },
            Message {
                kind: MsgKind::ReadMarker,
                ..Message::default()
            },
            Message {
                reply_to: Some((String::new(), "an earlier message".into())),
                ..base("alice", "A reply.")
            },
            Message {
                state: SendState::Sending,
                own: true,
                ..base("you", "On its way")
            },
            Message {
                state: SendState::Failed,
                own: true,
                ..base("you", "Did not make it")
            },
        ];
        insta::assert_snapshot!(render(&mut app, 70, 26));
    }

    #[test]
    fn formatted_messages() {
        use crate::model::Message;
        let mut app = App::demo();
        let html = |html: &str| Message {
            sender: "alice".into(),
            time: "12:00".into(),
            day: "Today".into(),
            body: "plain fallback".into(),
            html: Some(html.into()),
            ..Message::default()
        };
        let room = &mut app.rooms[0];
        room.typing = None;
        room.messages = vec![
            html(
                "Some <strong>bold</strong>, <em>italic</em>, <del>struck</del> and <code>code</code>.",
            ),
            html(
                "<blockquote><p>A quote that is long enough to need wrapping onto a second row here.</p></blockquote><p>And the answer.</p>",
            ),
            html("<ul><li>first</li><li>second</li></ul><ol><li>one</li><li>two</li></ol>"),
            html(
                "<pre><code class=\"language-rust\">fn main() {\n    println!(\"hi\");\n}\n</code></pre>",
            ),
            html(
                "A <a href=\"https://example.org\">link</a> and a <span data-mx-spoiler>secret</span>.",
            ),
        ];
        insta::assert_snapshot!("formatted_covered", render(&mut app, 64, 30));
        // Selecting the message uncovers its spoiler.
        app.perform(Action::SelectOlder);
        insta::assert_snapshot!("formatted_revealed", render(&mut app, 64, 30));
    }

    #[test]
    fn login_form() {
        let mut app = App::demo();
        app.rooms.clear();
        app.on_matrix(crate::matrix::MatrixEvent::NeedLogin { reason: None });
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn reauth_login_modes_render_fixed_identity() {
        use crate::matrix::{MatrixEvent, ReauthKind, ReauthTarget};
        let mut app = App::demo();
        app.rooms.clear();

        app.on_matrix(MatrixEvent::NeedReauth {
            target: ReauthTarget {
                homeserver: "https://example.org".into(),
                user_id: "@alice:example.org".into(),
                device_id: "DEV1".into(),
                kind: ReauthKind::Password,
                client_id: None,
            },
            reason: "Session expired; log in again to keep this device.".into(),
        });
        let rendered_pw = render(&mut app, 100, 30);
        assert!(rendered_pw.contains("Re-authenticate session"));
        assert!(rendered_pw.contains("https://example.org"));
        assert!(rendered_pw.contains("@alice:example.org"));
        assert!(rendered_pw.contains("Password"));
        assert!(rendered_pw.contains("Log in"));
        assert!(!rendered_pw.contains("Log in with browser"));

        app.on_matrix(MatrixEvent::NeedReauth {
            target: ReauthTarget {
                homeserver: "https://sso.example.org".into(),
                user_id: "@alice:sso.example.org".into(),
                device_id: "DEV2".into(),
                kind: ReauthKind::Browser,
                client_id: Some("cid".into()),
            },
            reason: "Session expired; approve in browser to keep this device.".into(),
        });
        let rendered_br = render(&mut app, 100, 30);
        assert!(rendered_br.contains("Re-authenticate session"));
        assert!(rendered_br.contains("@alice:sso.example.org"));
        assert!(rendered_br.contains("Log in with browser"));
        assert!(!rendered_br.contains("Password"));
    }

    #[test]
    fn command_palette() {
        let mut app = App::demo();
        app.perform(Action::OpenPalette);
        insta::assert_snapshot!(render(&mut app, 100, 30));
        insta::assert_snapshot!("command_palette_narrow", render(&mut app, 46, 16));
        for c in "rat".chars() {
            app.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(c),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        insta::assert_snapshot!("command_palette_filtered", render(&mut app, 100, 30));
    }

    #[test]
    fn global_search() {
        use crate::matrix::SearchHit;
        use crate::modal::Modal;

        let mut app = App::demo();
        app.perform(Action::SearchEverywhere);
        insta::assert_snapshot!("global_search_empty", render(&mut app, 100, 30));

        if let Some(Modal::GlobalSearch(s)) = &mut app.modal {
            s.query.set("deadline");
            s.active_query = "deadline".into();
            s.hits = vec![
                SearchHit {
                    room_id: "!design:example.org".into(),
                    event_id: "$1".into(),
                    thread_root: None,
                    room_name: "Design".into(),
                    sender: "Nora".into(),
                    date: "Today".into(),
                    snippet: "We should keep the Friday deadline after all".into(),
                },
                SearchHit {
                    room_id: "!ops:example.org".into(),
                    event_id: "$2".into(),
                    thread_root: None,
                    room_name: "Ops & Releases".into(),
                    sender: "Ben".into(),
                    date: "12 Nov".into(),
                    snippet: "Wait until everyone signs off on the release checklist before moving the deadline into next week".into(),
                },
            ];
            s.selected = 0;
        }
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn composer_completions() {
        let mut app = App::demo();
        for c in "hey @".chars() {
            app.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(c),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn every_key_in_the_help_can_be_reached() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let mut app = App::demo();
        let last = app.keymap.bindings().last().unwrap().description;
        // When tall enough, everything is there at once; smaller windows scroll.
        app.perform(Action::ToggleHelp);
        let text = render(&mut app, 100, 60);
        for binding in app.keymap.bindings() {
            assert!(
                text.contains(binding.description),
                "{}",
                binding.description
            );
        }
        assert!(text.contains("Ctrl-Q      Quit"));
        // (The footer's "wheel scrolls" is not the scroll hint.)
        assert!(!text.contains("↑↓ scroll"));
        // In smaller windows what does not fit is a few presses away, in both
        // layouts, and the way back as well.
        for (w, h) in [(100, 32), (100, 30), (100, 24), (80, 24), (60, 12)] {
            let mut app = App::demo();
            app.perform(Action::ToggleHelp);
            let text = render(&mut app, w, h);
            assert!(text.contains("↑↓ scroll"), "{w}x{h}");
            assert!(!text.contains(last), "{w}x{h}");
            let mut seen = text;
            for _ in 0..80 {
                app.on_event(key(KeyCode::Down));
                seen.push_str(&render(&mut app, w, h));
            }
            for binding in app.keymap.bindings() {
                assert!(
                    seen.contains(binding.description),
                    "{w}x{h}: {}",
                    binding.description
                );
            }
            assert!(render(&mut app, w, h).contains(last), "{w}x{h}");
            // Not scrolled past the end: one step up changes the view.
            let bottom = render(&mut app, w, h);
            app.on_event(key(KeyCode::Up));
            assert_ne!(render(&mut app, w, h), bottom, "{w}x{h}");
            app.on_event(key(KeyCode::Home));
            assert!(render(&mut app, w, h).contains("Next pane"), "{w}x{h}");
            // Opened again, it starts at the top.
            app.on_event(key(KeyCode::End));
            app.on_event(key(KeyCode::Esc));
            assert!(!app.help_open);
            app.perform(Action::ToggleHelp);
            assert_eq!(app.help_scroll, 0);
        }
    }

    /// The context menu of one of our own messages, with Delete armed or not, and a
    /// title, hanging from the far corner.
    fn menu_modal(armed: bool) -> Option<crate::modal::Modal> {
        use crate::menu::{Anchor, Menu, MessageFacts, Target, message_items};
        let message = crate::model::Message {
            body: "hello".into(),
            own: true,
            ..Default::default()
        };
        let keys = crate::keymap::Keymap::default();
        let items = message_items(&message, MessageFacts::default(), &keys);
        let target = Target::Composer {
            room_id: "rust".into(),
        };
        let mut menu = Menu::new(target, "A title", items, Anchor::at(u16::MAX, u16::MAX))?;
        if armed {
            let delete = menu.items.len() - 1;
            assert!(!menu.confirmed(delete));
        }
        Some(crate::modal::Modal::Menu(menu))
    }

    #[test]
    fn tiny_sizes_do_not_panic() {
        use crate::modal::{EmojiPicker, LoginForm, Modal, RecoveryForm, SettingsMenu};
        let mut with_palette = App::demo();
        with_palette.perform(Action::OpenPalette);
        let palette_modal = with_palette.modal.take();
        let mut with_global_search = App::demo();
        with_global_search.perform(Action::SearchEverywhere);
        let global_search_modal = with_global_search.modal.take();
        use crate::manage::{InfoPopup, NewPopup, Stage};
        let mut create = NewPopup::new(Stage::Create);
        create.error = Some("an error".into());
        let mut with_info = App::demo();
        with_info.perform(Action::RoomInfo);
        let Some(Modal::RoomInfo(mut info)) = with_info.modal.take() else {
            panic!("room info did not open");
        };
        info.editing = Some((crate::manage::InfoRow::Topic, "a topic".into()));
        let _: &InfoPopup = &info;
        let with_keybinds = Some(Modal::Keybinds(crate::keymap::Editor::new(
            &crate::keymap::Keymap::default(),
        )));
        let modals = [
            None,
            Some(Modal::Emoji(EmojiPicker::default())),
            Some(Modal::Settings(SettingsMenu::default())),
            Some(Modal::Login(LoginForm::new(Some("an error".into())))),
            Some(Modal::Login(LoginForm::reauth(
                crate::matrix::ReauthTarget {
                    homeserver: "https://example.org".into(),
                    user_id: "@alice:example.org".into(),
                    device_id: "DEV1".into(),
                    kind: crate::matrix::ReauthKind::Password,
                    client_id: None,
                },
                "Session expired.".into(),
            ))),
            Some(Modal::Login(LoginForm::reauth(
                crate::matrix::ReauthTarget {
                    homeserver: "https://sso.example.org".into(),
                    user_id: "@alice:sso.example.org".into(),
                    device_id: "DEV2".into(),
                    kind: crate::matrix::ReauthKind::Browser,
                    client_id: Some("cid".into()),
                },
                "Session expired.".into(),
            ))),
            Some(Modal::Recovery(RecoveryForm::default())),
            palette_modal,
            global_search_modal,
            with_keybinds,
            Some(Modal::New(Box::new(NewPopup::new(Stage::Choose)))),
            Some(Modal::New(Box::new(NewPopup::new(Stage::Join)))),
            Some(Modal::New(Box::new(create))),
            Some(Modal::RoomInfo(info)),
            menu_modal(false),
            menu_modal(true),
            {
                let mut pinned =
                    crate::modal::PinnedPopup::open("!r:example.org".into(), "rust".into(), true);
                pinned.set_items(
                    vec![crate::model::PinnedMessage {
                        event_id: "$p1:example.org".into(),
                        sender: "carol".into(),
                        sender_id: Some("@carol:example.org".into()),
                        day: "Today".into(),
                        time: "09:12".into(),
                        snippet: "Pinned message snippet".into(),
                        redacted: false,
                        undecryptable: false,
                        ignored: false,
                    }],
                    None,
                );
                Some(Modal::Pinned(pinned))
            },
            Some(Modal::Threads(crate::modal::ThreadsPopup::new(
                "rust".into(),
                "rust".into(),
                1,
                vec![crate::model::ThreadInfo {
                    root: "demo:0".into(),
                    sender: "alice".into(),
                    text: "A thread".into(),
                    replies: 2,
                    latest_sender: Some("bob".into()),
                    latest_when: Some("10:32".into()),
                    unread: true,
                    mine: false,
                }],
                false,
            ))),
            Some(Modal::ReasonConfirm(
                crate::modal::ReasonConfirmPopup::redact(
                    "!r:example.org".into(),
                    crate::model::ItemRef::Event("$p1:example.org".into()),
                    "carol".into(),
                    "Off-topic preview text".into(),
                ),
            )),
            Some(Modal::ReasonConfirm(crate::modal::ReasonConfirmPopup::ban(
                "!r:example.org".into(),
                "@bob:example.org".into(),
                "bob".into(),
            ))),
        ];
        for modal in modals {
            let mut app = App::demo();
            app.perform(Action::ToggleHelp);
            app.modal = modal;
            for (w, h) in [
                (1, 1),
                (3, 2),
                (10, 4),
                (20, 3),
                (25, 7),
                (61, 5),
                (200, 2),
                (80, 24),
            ] {
                render(&mut app, w, h);
                if app.modal.is_none() {
                    app.perform(Action::FocusNext);
                }
            }
        }
        // Also exercise the completion popup across all tiny sizes.
        let mut completed = App::demo();
        for c in "@".chars() {
            completed.on_event(crossterm::event::Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(c),
                    crossterm::event::KeyModifiers::NONE,
                ),
            ));
        }
        for (w, h) in [
            (1, 1),
            (3, 2),
            (10, 4),
            (20, 3),
            (25, 7),
            (61, 5),
            (200, 2),
            (80, 24),
        ] {
            render(&mut completed, w, h);
        }
    }

    #[test]
    fn triage_sidebar_spaces_low_priority_and_filters() {
        use crate::action::Action;
        use crate::matrix::SpaceSummary;
        use crate::model::{Room, RoomKind};

        let mut app = App::demo();
        let mut quiet = Room::new("!quiet:example.org", "archive", RoomKind::LowPriority);
        quiet.unread = 1;
        quiet.bold = true;
        let mut direct_low = Room::new("!dm_low:example.org", "erin", RoomKind::LowPriority);
        direct_low.direct = true;
        direct_low.unread = 2;
        direct_low.bold = true;
        app.rooms.push(quiet);
        app.rooms.push(direct_low);
        app.spaces = vec![
            SpaceSummary {
                id: "!work:example.org".into(),
                name: "Work".into(),
                level: 0,
                rooms: vec!["rust".into(), "bob".into(), "!dm_low:example.org".into()],
                ..Default::default()
            },
            SpaceSummary {
                id: "!misc:example.org".into(),
                name: "Misc".into(),
                level: 0,
                rooms: vec!["design".into(), "!quiet:example.org".into()],
                ..Default::default()
            },
        ];
        app.select_space(Some(0));
        app.perform(Action::FilterUnread);
        app.timeline.detached = true;
        app.timeline.detached_new = 3;
        app.timeline.reached_end = false;
        app.perform(Action::SelectOlder);
        app.perform(Action::SelectOlder);
        insta::assert_snapshot!(render(&mut app, 100, 30));
    }

    #[test]
    fn narrow_sidebar_quick_filters() {
        use crate::action::Action;

        let mut app = App::demo();
        app.sidebar.width = 16;
        app.perform(Action::FilterUnread);
        app.perform(Action::FilterPeople);
        insta::assert_snapshot!(render(&mut app, 80, 24));
    }

    #[test]
    fn moderation_modals_and_pinned_toast_snapshots() {
        use crate::action::Action;
        use crate::matrix::{MemberRole, RoomDetails, RoomMember};
        use crate::modal::Modal;
        use crate::model::PinnedMessage;

        // Pinned messages popup (normal + redacted pin).
        let mut pinned_app = App::demo();
        pinned_app.perform(Action::OpenPinned);
        if let Some(Modal::Pinned(ref mut popup)) = pinned_app.modal {
            popup.can_pin = true;
            popup.set_items(
                vec![
                    PinnedMessage {
                        event_id: "$1".into(),
                        sender: "carol".into(),
                        sender_id: Some("@carol:example.org".into()),
                        day: "Today".into(),
                        time: "09:12".into(),
                        snippet: "Is the sliding sync proxy still needed?".into(),
                        redacted: false,
                        undecryptable: false,
                        ignored: false,
                    },
                    PinnedMessage {
                        event_id: "$2".into(),
                        sender: "bob".into(),
                        sender_id: Some("@bob:example.org".into()),
                        day: "Yesterday".into(),
                        time: "18:04".into(),
                        snippet: String::new(),
                        redacted: true,
                        undecryptable: false,
                        ignored: false,
                    },
                ],
                None,
            );
        }
        insta::assert_snapshot!("pinned_popup", render(&mut pinned_app, 100, 30));

        // ReasonConfirmPopup (redact).
        let mut redact_app = App::demo();
        let mut redact_modal = crate::modal::ReasonConfirmPopup::redact(
            redact_app.rooms[redact_app.active].id.clone(),
            crate::model::ItemRef::Event("$1".into()),
            "carol".into(),
            "Morning! Is the sliding sync proxy still needed?".into(),
        );
        redact_modal.reason.set("duplicate question");
        redact_app.modal = Some(Modal::ReasonConfirm(redact_modal));
        insta::assert_snapshot!("reason_confirm_redact", render(&mut redact_app, 100, 30));

        // RoomInfo showing Banned section, custom power level, and inline BanPrompt.
        let mut info_app = App::demo();
        info_app.perform(Action::RoomInfo);
        if let Some(Modal::RoomInfo(ref mut info)) = info_app.modal {
            info.details = Some(RoomDetails {
                name: "rust".into(),
                topic: "Matrix TUI dev".into(),
                joined: 3,
                own_power_level: 100,
                admin_count: 1,
                members: vec![
                    RoomMember {
                        user_id: "@alice:example.org".into(),
                        name: "alice".into(),
                        role: MemberRole::Admin,
                        power_level: 100,
                        own: true,
                        role_editable: true,
                        ..RoomMember::default()
                    },
                    RoomMember {
                        user_id: "@carol:example.org".into(),
                        name: "carol".into(),
                        role: MemberRole::Moderator,
                        power_level: 75,
                        bannable: true,
                        role_editable: true,
                        ..RoomMember::default()
                    },
                    RoomMember {
                        user_id: "@eve:example.org".into(),
                        name: "eve".into(),
                        role: MemberRole::User,
                        power_level: 0,
                        banned: true,
                        unbannable: true,
                        ..RoomMember::default()
                    },
                ],
                ..RoomDetails::default()
            });
            info.select(crate::manage::InfoRow::Member(1));
            info.ban_prompt = Some(crate::manage::BanPrompt::new(
                "@carol:example.org".into(),
                "carol".into(),
            ));
        }
        insta::assert_snapshot!("room_info_moderation", render(&mut info_app, 100, 30));

        // Timeline "(pinned)" badge + Forwarded toast with "Open ->" button.
        let mut toast_app = App::demo();
        if let Some(first) = toast_app.rooms[toast_app.active].messages.get_mut(1) {
            first.pinned = true;
        }
        let target_id = toast_app.rooms[1].id.clone();
        toast_app.on_matrix(crate::matrix::MatrixEvent::ForwardResult {
            to_room_id: target_id,
            to_room_name: "design".into(),
            result: Ok(()),
        });
        insta::assert_snapshot!(
            "pinned_badge_and_forward_toast",
            render(&mut toast_app, 100, 30)
        );
    }

    #[test]
    fn the_emoji_picker_stays_on_screen_when_the_composer_is_not_drawn() {
        use crate::modal::{EmojiPicker, Modal};
        use crate::ui::hit::HitId;

        // Drawn above the composer of a large window.
        let mut app = App::demo();
        app.modal = Some(Modal::Emoji(EmojiPicker::default()));
        render(&mut app, 120, 40);
        assert!(app.composer_view.area.y > 30);

        // The window shrinks to one pane, and that is the room list: there is no
        // composer to sit by, and where it was is far below the window now.
        app.focus = Pane::Sidebar;
        render(&mut app, 50, 14);
        let picker = app
            .hits
            .rect_of(HitId::Overlay)
            .expect("the picker is drawn");
        assert_eq!(picker.intersection(app.screen), picker, "{picker:?}");
        // Nor is anything else placed by it.
        assert!(app.composer_view.area.is_empty());
        assert!(app.composer_view.cursor.is_none());
    }
}
