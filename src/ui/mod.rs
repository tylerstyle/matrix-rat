//! Drawing. Each frame also rebuilds the hit map used for mouse handling.

mod attach;
mod complete;
mod composer;
mod global_search;
mod help;
pub mod hit;
mod image;
mod keybinds;
mod menu;
mod modal;
mod palette;
mod roominfo;
mod search;
mod sidebar;
mod status;
mod timeline;
mod verify;

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
pub const SIDEBAR_MIN: u16 = 16;

pub fn draw(frame: &mut Frame, app: &mut App) {
    app.hits.clear();
    app.images.begin_frame();
    let area = frame.area();
    app.screen = area;
    frame
        .buffer_mut()
        .set_style(area, app.theme.style(Role::Text));

    let [main, status_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

    let narrow = area.width < NARROW_WIDTH;
    let (sidebar_area, chat_area) = if narrow {
        if app.focus == Pane::Sidebar {
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
    if let Some(area) = chat_area {
        let composer_height = composer::height(app, area.width);
        let [timeline_area, composer_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(composer_height)])
                .areas(area);
        timeline::draw(frame, app, timeline_area);
        composer::draw(frame, app, composer_area);
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
    }

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
        let text = render(&mut app, 100, 34);
        for binding in app.keymap.bindings() {
            assert!(
                text.contains(binding.description),
                "{}",
                binding.description
            );
        }
        assert!(text.contains("Ctrl-Q      Quit"));
        assert!(!text.contains("scroll"));
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
            Some(Modal::New(NewPopup::new(Stage::Choose))),
            Some(Modal::New(NewPopup::new(Stage::Join))),
            Some(Modal::New(create)),
            Some(Modal::RoomInfo(info)),
            menu_modal(false),
            menu_modal(true),
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
            },
            SpaceSummary {
                id: "!misc:example.org".into(),
                name: "Misc".into(),
                level: 0,
                rooms: vec!["design".into(), "!quiet:example.org".into()],
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
}
