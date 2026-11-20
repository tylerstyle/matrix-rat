//! The rooms bar: one line above the chat while the room list is out of sight (put
//! away, or the window is narrow and a chat is open). Its label leads back to the
//! room list; after it come the rooms with new notifications, the newest on the left,
//! each as the tile the room list shows for it with its count behind it. What does not
//! fit is counted at the end: `(3)>`.
//!
//! A bar of one line has no "top right" of a tile to put the count at, so the count
//! is written in superscript digits directly behind the tile, in the colours of the
//! room list's badge.
//!
//! For the pointer or a finger: a room's tile, its count and the gap behind it open
//! that room; the label and the `(n)>` lead to the room list. The empty rest of the
//! line does nothing: it is too easy to hit by accident.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::HitId;
use super::sidebar::{badge_role, count_label, initial, tile_style};
use crate::action::Action;
use crate::app::App;
use crate::text::{truncate, width};
use crate::theme::Role;

const LABEL: &str = "Rooms";
/// The sign the status line uses for the room list as well (two columns wide).
const ICON: &str = "☰";
/// A room's tile, as in the room list with one line per room.
const TILE: u16 = 2;
/// The gap behind a room's count. It belongs to the room, for a wider target.
const GAP: u16 = 1;
/// The bar never takes the last lines from the messages: this many stay.
const MESSAGE_ROWS: u16 = 3;

/// How many lines the bar takes from the chat area: one, or none where it is not
/// shown (the setting is off, the room list is in view, or the window is too low).
pub fn height(app: &App, one_pane: bool, chat: Rect) -> u16 {
    if !app.rooms_bar || !one_pane || chat.width == 0 {
        return 0;
    }
    // Under the bar: the messages in their frame of two lines, and the composer
    // as high as it is right now.
    let needed = super::composer::height(app, chat.width)
        .saturating_add(2)
        .saturating_add(MESSAGE_ROWS);
    u16::from(chat.height > needed)
}

/// A count in superscript digits ("12" becomes "¹²", "99+" becomes "⁹⁹⁺").
fn superscript(count: &str) -> String {
    count
        .chars()
        .map(|c| match c {
            '0' => '⁰',
            '1' => '¹',
            '2' => '²',
            '3' => '³',
            '4' => '⁴',
            '5' => '⁵',
            '6' => '⁶',
            '7' => '⁷',
            '8' => '⁸',
            '9' => '⁹',
            '+' => '⁺',
            other => other,
        })
        .collect()
}

/// What of the bar fits its width.
#[derive(Debug, PartialEq, Eq)]
struct Plan {
    /// The label as it is drawn, with its sign and margins, cut where it has to be.
    label: String,
    /// How many of the rooms are drawn, from the left.
    shown: usize,
    /// `(n)>` for the `n` rooms that are not.
    more: Option<String>,
}

/// The sign alone in at most `budget` columns, with its margins where they fit.
fn sign_text(budget: usize) -> String {
    let sign = width(ICON);
    if budget >= sign + 2 {
        format!(" {ICON} ")
    } else if budget >= sign {
        ICON.to_owned()
    } else {
        String::new()
    }
}

/// The label in at most `budget` columns: whole, else cut, else the sign alone.
fn label_text(label: &str, budget: usize) -> String {
    let full = format!(" {ICON} {label} ");
    if width(&full) <= budget {
        return full;
    }
    // The sign, the margins and the gap between sign and text.
    let fixed = width(ICON) + 3;
    let cut = truncate(label, budget.saturating_sub(fixed));
    if cut.is_empty() || cut == "…" {
        sign_text(budget)
    } else {
        format!(" {ICON} {cut} ")
    }
}

/// As many of the rooms as fit `rest` columns, with the `(n)>` for the others: how
/// many, the `(n)>`, and the columns they take. More columns never show fewer rooms.
fn fit(rooms: &[u16], rest: u16) -> (usize, Option<String>, u16) {
    let wanted = rooms.iter().fold(0u16, |sum, w| sum.saturating_add(*w));
    // The last room's gap may fall off the edge.
    let all = wanted.saturating_sub(GAP);
    if all <= rest {
        return (rooms.len(), None, all);
    }
    // `before[n]`: the columns of the first `n` rooms.
    let mut before: Vec<u16> = Vec::with_capacity(rooms.len());
    let mut sum = 0u16;
    for w in rooms {
        before.push(sum);
        sum = sum.saturating_add(*w);
    }
    for shown in (0..rooms.len()).rev() {
        let more = format!("({})>", rooms.len() - shown);
        let used = before[shown].saturating_add(width(&more) as u16);
        if used <= rest {
            return (shown, Some(more), used);
        }
    }
    (0, None, 0)
}

/// Lays the bar out in `bar` columns for rooms of these widths.
///
/// The sign of the label is the way back to the room list and always there. The
/// rooms come next: as many as fit beside the sign, with the `(n)>` for the rest.
/// The label's text has what they leave, whole or cut. So a wider bar never shows
/// fewer rooms, which a label that kept a share of its own did.
fn plan(label: &str, rooms: &[u16], bar: u16) -> Plan {
    let sign = width(&sign_text(bar as usize)) as u16;
    let (shown, more, used) = fit(rooms, bar.saturating_sub(sign));
    Plan {
        label: label_text(label, bar.saturating_sub(used) as usize),
        shown,
        more,
    }
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    draw_labelled(frame, app, area, LABEL);
}

fn draw_labelled(frame: &mut Frame, app: &mut App, area: Rect, label: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let area = Rect::new(area.x, area.y, area.width, 1);
    let theme = &app.theme;
    frame
        .buffer_mut()
        .set_style(area, theme.style(Role::Status));
    // The label and the `(n)>` lead to the room list.
    let to_list = HitId::Do(Action::CloseChat);
    let list_style = if app.hover == Some(to_list) {
        theme.style(Role::Hover)
    } else {
        theme.style(Role::StatusKey)
    };

    let rooms: Vec<(usize, String)> = app
        .active_rooms()
        .into_iter()
        .map(|index| (index, superscript(&count_label(app.rooms[index].unread))))
        .collect();
    let widths: Vec<u16> = rooms
        .iter()
        .map(|(_, count)| TILE + width(count) as u16 + GAP)
        .collect();
    let plan = plan(label, &widths, area.width);

    let label_width = (width(&plan.label) as u16).min(area.width);
    frame.render_widget(
        Line::from(Span::styled(plan.label, list_style)),
        Rect::new(area.x, area.y, label_width, 1),
    );
    app.hits
        .add(Rect::new(area.x, area.y, label_width, 1), to_list);

    // The pictures are those of the room list, at its one-line size. The list is
    // not drawn while the bar is, so the two never ask for different sizes in one
    // frame.
    // With room previews on the list wants them larger, so going between list and
    // chat encodes them anew each time: left for the pass over memory and CPU.
    app.images.set_avatar_cells((TILE, 1));
    let covered = app.modal.is_some() || app.help_open || app.images.viewer_open();
    let mut x = area.x + label_width;
    for ((index, count), room_width) in rooms.iter().zip(&widths).take(plan.shown) {
        let room = &app.rooms[*index];
        let rect = Rect::new(x, area.y, *room_width, 1).intersection(area);
        let line = Line::from(vec![
            Span::styled(initial(&room.name), tile_style(theme, &room.name)),
            Span::styled(count.as_str(), theme.style(badge_role(room.mention))),
        ]);
        frame.render_widget(&line, rect);
        if app.hover == Some(HitId::BarRoom(*index)) {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        // The room's picture goes over the tile where the terminal can show one.
        if rect.width >= TILE
            && let Some(url) = &room.avatar
        {
            let cells = Rect::new(rect.x, rect.y, TILE, 1);
            super::image::draw_avatar(frame, &mut app.images, theme, url, cells, 0, covered);
        }
        app.hits.add(rect, HitId::BarRoom(*index));
        x = x.saturating_add(*room_width);
    }
    if let Some(more) = plan.more
        && x < area.right()
    {
        let rect = Rect::new(x, area.y, width(&more) as u16, 1).intersection(area);
        frame.render_widget(Line::from(Span::styled(more, list_style)), rect);
        app.hits.add(rect, to_list);
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::action::Pane;
    use crate::model::{Room, RoomKind};

    fn room(name: &str, unread: u32) -> Room {
        let mut room = Room::new(name, name, RoomKind::Room);
        room.unread = unread;
        room
    }

    /// The demo with the room list put away, and these rooms after the open one.
    fn app_with(rooms: Vec<Room>) -> App {
        let mut app = App::demo();
        app.rooms.truncate(1);
        app.rooms.extend(rooms);
        app.sidebar.hidden = true;
        app
    }

    fn terminal(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, app))
            .unwrap();
        terminal
    }

    /// The first lines of the screen: the bar, and the top of what is under it.
    fn top(app: &mut App, width: u16, height: u16, lines: usize) -> String {
        let screen = format!("{}", terminal(app, width, height).backend());
        screen.lines().take(lines).collect::<Vec<_>>().join("\n")
    }

    fn bar(app: &mut App, width: u16) -> String {
        top(app, width, 12, 1)
    }

    /// The column where `needle` starts in the first line.
    fn column(terminal: &Terminal<TestBackend>, needle: &str) -> u16 {
        let buffer = terminal.backend().buffer();
        let mut line = String::new();
        let mut columns = Vec::new();
        for x in 0..buffer.area.width {
            let symbol = buffer[(x, 0)].symbol();
            for _ in symbol.chars() {
                columns.push(x);
            }
            line.push_str(symbol);
        }
        let at = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in {line:?}"));
        columns[line[..at].chars().count()]
    }

    fn press(button: MouseButton, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(button),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn click(column: u16) -> Event {
        press(MouseButton::Left, column, 0)
    }

    fn open_name(app: &App) -> Option<&str> {
        app.room().map(|room| room.name.as_str())
    }

    #[test]
    fn superscript_counts_take_one_column_a_digit() {
        assert_eq!(superscript("1234567890"), "¹²³⁴⁵⁶⁷⁸⁹⁰");
        assert_eq!(superscript(&count_label(7)), "⁷");
        assert_eq!(superscript(&count_label(100)), "⁹⁹⁺");
        assert_eq!(superscript(&count_label(u32::MAX)), "⁹⁹⁺");
        for count in ["1234567890", "99+"] {
            assert_eq!(width(&superscript(count)), count.len(), "{count}");
        }
    }

    #[test]
    fn the_plan_shows_what_fits_and_counts_the_rest() {
        let of = |label: &str, shown: usize, more: Option<&str>| Plan {
            label: label.to_owned(),
            shown,
            more: more.map(str::to_owned),
        };
        // No rooms: the label alone.
        assert_eq!(plan("Rooms", &[], 40), of(" ☰ Rooms ", 0, None));
        // All fit; the last one's gap may fall off the edge.
        assert_eq!(plan("Rooms", &[4, 4, 5], 23), of(" ☰ Rooms ", 3, None));
        assert_eq!(plan("Rooms", &[4, 4, 5], 22), of(" ☰ Rooms ", 3, None));
        // Narrower: the label's text gives way to the rooms, down to the sign.
        assert_eq!(plan("Rooms", &[4, 4, 5], 21), of(" ☰ Roo… ", 3, None));
        assert_eq!(plan("Rooms", &[4, 4, 5], 16), of(" ☰ ", 3, None));
        // Then the rooms do, and what is left out is counted.
        assert_eq!(plan("Rooms", &[4, 4, 5], 15), of(" ☰ R… ", 1, Some("(2)>")));
        assert_eq!(plan("Rooms", &[4, 4, 5], 12), of(" ☰ ", 1, Some("(2)>")));
        assert_eq!(plan("Rooms", &[4, 4, 5], 11), of(" ☰ R… ", 0, Some("(3)>")));
        assert_eq!(plan("Rooms", &[4, 4, 5], 8), of(" ☰ ", 0, Some("(3)>")));
        // The number is that of the rooms not shown.
        assert_eq!(plan("Rooms", &[4; 40], 30), of(" ☰ ", 5, Some("(35)>")));
        // The sign stays where nothing else fits.
        assert_eq!(plan("Rooms", &[4; 3], 7), of(" ☰ R… ", 0, None));
        assert_eq!(plan("Rooms", &[4; 3], 6), of(" ☰ ", 0, None));
        assert_eq!(plan("Rooms", &[4; 3], 3), of("☰", 0, None));
        assert_eq!(plan("Rooms", &[4; 3], 2), of("☰", 0, None));
        assert_eq!(plan("Rooms", &[4; 3], 1), of("", 0, None));
        assert_eq!(plan("Rooms", &[4; 3], 0), of("", 0, None));
        // Without rooms the label has the whole bar.
        assert_eq!(plan("Rooms", &[], 10), of(" ☰ Rooms ", 0, None));
        assert_eq!(plan("Rooms", &[], 9), of(" ☰ Roo… ", 0, None));
        // Widths that would overflow a `u16` do not.
        assert_eq!(
            plan("Rooms", &[u16::MAX; 3], 30),
            of(" ☰ Rooms ", 0, Some("(3)>"))
        );
    }

    #[test]
    fn a_wider_bar_never_shows_fewer_rooms_and_nothing_is_wider_than_the_bar() {
        let mixed: Vec<u16> = (0..60).map(|n| 4 + (n * 7 % 3)).collect();
        let sets: [&[u16]; 9] = [
            &[],
            &[4],
            &[6],
            &[4, 4, 5],
            &[4; 3],
            &[6, 5, 4, 4, 6, 5],
            &[5; 12],
            &[4; 200],
            &mixed,
        ];
        let long = "Rooms and everything else that could ever be said about them";
        for label in ["Rooms", "", "部屋", long] {
            for rooms in sets {
                let mut before = 0;
                for bar in 0..=130u16 {
                    let plan = plan(label, rooms, bar);
                    let what = format!("{label:?} {rooms:?} at {bar}: {plan:?}");
                    // Never fewer rooms than in a narrower bar.
                    assert!(plan.shown >= before, "{what}");
                    before = plan.shown;
                    assert!(plan.shown <= rooms.len(), "{what}");
                    // The number is exactly that of the rooms not shown, and it is
                    // there whenever a room is shown and another is not.
                    let hidden = rooms.len() - plan.shown;
                    match &plan.more {
                        Some(more) => {
                            assert!(hidden > 0, "{what}");
                            assert_eq!(*more, format!("({hidden})>"), "{what}");
                        }
                        None => assert!(hidden == 0 || plan.shown == 0, "{what}"),
                    }
                    // Label, rooms and the count together fit the bar; only the
                    // last room's gap may fall off the edge.
                    let drawn: u16 = rooms[..plan.shown].iter().sum();
                    let more = plan.more.as_deref().map_or(0, width) as u16;
                    let total = width(&plan.label) as u16 + drawn + more;
                    let spare = if hidden == 0 && plan.shown > 0 {
                        GAP
                    } else {
                        0
                    };
                    assert!(total <= bar + spare, "{what}");
                    // The sign is there from the width it fits in.
                    assert_eq!(plan.label.contains(ICON), bar >= 2, "{what}");
                }
                // Wide enough, everything is there and whole.
                let all: u16 = rooms.iter().sum();
                let wide = plan(label, rooms, all + width(label) as u16 + 5);
                assert_eq!((wide.shown, wide.more), (rooms.len(), None));
                assert_eq!(wide.label, format!(" {ICON} {label} "));
            }
        }
    }

    #[test]
    fn a_very_long_label_is_cut_and_leaves_the_rooms_their_place() {
        let long = "Rooms and everything else that could ever be said about them";
        let cut = plan(long, &[4; 3], 40);
        assert_eq!(width(&cut.label), 29);
        assert!(cut.label.ends_with("… "), "{cut:?}");
        assert_eq!((cut.shown, cut.more.as_deref()), (3, None));
        let cut = plan(long, &[4; 30], 40);
        assert_eq!(cut.label, " ☰ R… ");
        assert_eq!((cut.shown, cut.more.as_deref()), (7, Some("(23)>")));
        // Wide characters are cut at a whole one, or the sign stands alone.
        let wide = plan("部屋の一覧はこちら", &[4; 30], 24);
        assert_eq!(wide.label, " ☰ ");
        let wide = plan("部屋の一覧はこちら", &[4; 2], 24);
        assert_eq!(wide.label, " ☰ 部屋の一覧… ");

        // Drawn: the same, in every width, with nothing outside the line.
        let mut app = app_with(vec![room("bob", 2), room("design", 5), room("nixos", 12)]);
        let mut lines = Vec::new();
        for bar_width in [60, 40, 24, 12, 5, 1] {
            let mut terminal = Terminal::new(TestBackend::new(bar_width, 2)).unwrap();
            terminal
                .draw(|frame| {
                    let area = Rect::new(0, 0, bar_width, 1);
                    draw_labelled(frame, &mut app, area, long);
                })
                .unwrap();
            let screen = format!("{}", terminal.backend());
            let mut rows = screen.lines();
            lines.push(rows.next().unwrap_or_default().to_owned());
            // The line under it is left alone.
            let under = rows.next().unwrap_or_default();
            assert_eq!(under.trim_matches(['"', ' ']), "", "{bar_width}");
        }
        insta::assert_snapshot!("rooms_bar_long_label", lines.join("\n"));
    }

    #[test]
    fn the_bar_is_above_the_chat_in_a_narrow_window() {
        let mut app = App::demo();
        insta::assert_snapshot!("rooms_bar_narrow", top(&mut app, 50, 20, 3));
    }

    #[test]
    fn the_bar_is_above_the_chat_while_the_room_list_is_put_away() {
        let mut app = App::demo();
        app.perform(Action::ToggleSidebar);
        insta::assert_snapshot!("rooms_bar_list_hidden", top(&mut app, 100, 30, 3));
    }

    #[test]
    fn with_no_active_room_the_bar_still_leads_to_the_room_list() {
        let mut app = app_with(vec![room("quiet", 0), room("calm", 0)]);
        insta::assert_snapshot!("rooms_bar_no_active_room", top(&mut app, 60, 12, 2));
        // The label with its margins is the target; the empty rest of the line is
        // none, and a click there changes nothing.
        let _ = terminal(&mut app, 60, 12);
        for x in 0..10 {
            assert_eq!(app.hits.at(x, 0), Some(HitId::Do(Action::CloseChat)), "{x}");
        }
        for x in [10, 11, 30, 59] {
            assert!(
                !matches!(
                    app.hits.at(x, 0),
                    Some(HitId::Do(_) | HitId::BarRoom(_) | HitId::Pane(_))
                ),
                "{x}"
            );
            app.on_event(click(x));
            assert!(!app.chat_closed(), "{x}");
            assert_eq!(app.focus, Pane::Composer, "{x}");
            assert_eq!(open_name(&app), Some("rust"), "{x}");
        }
        // The same behind the last room, where there are some.
        let mut app = App::demo();
        let shown = terminal(&mut app, 50, 20);
        let end = column(&shown, "¹²") + 3;
        assert!(matches!(app.hits.at(end - 1, 0), Some(HitId::BarRoom(_))));
        for x in [end, end + 1, 49] {
            app.on_event(click(x));
            assert!(!app.chat_closed(), "{x}");
            assert_eq!(open_name(&app), Some("rust"), "{x}");
        }
    }

    #[test]
    fn a_click_on_the_label_does_not_give_up_an_edit_in_progress() {
        let mut app = App::demo();
        app.perform(Action::EditLast);
        assert!(matches!(app.compose_mode, crate::app::ComposeMode::Edit(_)));
        let shown = terminal(&mut app, 50, 20);
        app.on_event(click(column(&shown, "Rooms")));
        assert!(!app.chat_closed());
        assert!(matches!(app.compose_mode, crate::app::ComposeMode::Edit(_)));
        assert_eq!(
            app.toast.as_ref().map(|(text, _)| text.as_str()),
            Some("Finish or cancel the edit first")
        );
    }

    #[test]
    fn with_no_rooms_at_all_a_click_on_the_label_goes_to_the_room_list() {
        let mut app = App::demo();
        app.rooms.clear();
        app.sidebar.hidden = true;
        let shown = terminal(&mut app, 80, 20);
        assert_eq!(app.focus, Pane::Composer);
        let line = top(&mut app, 80, 20, 1);
        assert!(line.contains("☰ Rooms"), "{line}");
        app.on_event(click(column(&shown, "Rooms")));
        assert_eq!(app.focus, Pane::Sidebar);
        assert!(!app.chat_closed());
        // The list is in the chat's place now, and the bar went with the chat.
        let line = top(&mut app, 80, 20, 1);
        assert!(!line.contains(ICON), "{line}");
    }

    #[test]
    fn more_active_rooms_than_fit_are_counted_at_the_end() {
        let rooms = (1..=30)
            .map(|n| room(&format!("{}-room", (b'a' + (n % 26) as u8) as char), n))
            .collect();
        let mut app = app_with(rooms);
        let mut lines = Vec::new();
        for bar_width in [100, 60, 40, 24] {
            lines.push(bar(&mut app, bar_width));
        }
        insta::assert_snapshot!("rooms_bar_overflow", lines.join("\n"));
    }

    #[test]
    fn narrow_bars_keep_the_label_and_never_draw_outside_the_line() {
        let mut app = app_with(vec![room("bob", 2), room("design", 5), room("nixos", 120)]);
        let mut lines = Vec::new();
        for bar_width in [30, 24, 20, 16, 12, 9, 6, 3, 2, 1] {
            lines.push(bar(&mut app, bar_width));
        }
        insta::assert_snapshot!("rooms_bar_narrow_widths", lines.join("\n"));
    }

    #[test]
    fn wide_characters_in_initials_keep_every_room_at_its_width() {
        let mut mention = room("ßtraße", 100);
        mention.mention = true;
        let mut app = app_with(vec![
            room("日本語", 3),
            room("🐀 rats", 1),
            mention,
            room("", 9),
            room("#matrix", 42),
        ]);
        insta::assert_snapshot!("rooms_bar_wide_initials", bar(&mut app, 60));
        // Every room starts where the one before it ended: two columns of tile,
        // the count, one of gap.
        let terminal = terminal(&mut app, 60, 12);
        let start = column(&terminal, "日");
        let expected = [
            (start, 1),
            (start + 4, 2),
            (start + 8, 3),
            (start + 14, 4),
            (start + 18, 5),
        ];
        for (x, index) in expected {
            assert_eq!(app.hits.at(x, 0), Some(HitId::BarRoom(index)), "{x}");
            assert_ne!(app.hits.at(x - 1, 0), Some(HitId::BarRoom(index)), "{x}");
        }
    }

    #[test]
    fn the_bar_uses_the_colours_of_the_room_list_in_every_theme() {
        for name in crate::theme::Theme::builtin_names() {
            let mut loud = room("design", 5);
            loud.mention = true;
            let mut app = app_with(vec![room("bob", 2), loud]);
            app.theme = crate::theme::Theme::builtin(&name).unwrap();
            let terminal = terminal(&mut app, 60, 12);
            let buffer = terminal.backend().buffer();
            let theme = &app.theme;
            let style_at = |x: u16| buffer[(x, 0)].style();
            let has = |x: u16, role: Role| {
                let (cell, wanted) = (style_at(x), theme.style(role));
                (wanted.fg.is_none() || cell.fg == wanted.fg)
                    && (wanted.bg.is_none() || cell.bg == wanted.bg)
            };
            // The label as the keys of the status line, on the status line's ground.
            assert!(has(column(&terminal, "Rooms"), Role::StatusKey), "{name}");
            assert!(has(59, Role::Status), "{name}");
            // The tile in the room's own colour, the count as the list's badge.
            let bob = column(&terminal, "B ");
            assert_eq!(style_at(bob).bg, Some(theme.user_color("bob")), "{name}");
            assert_eq!(style_at(bob).fg, tile_style(theme, "bob").fg, "{name}");
            assert!(has(column(&terminal, "²"), Role::UnreadBadge), "{name}");
            // A mention stands out as it does in the list.
            assert!(has(column(&terminal, "⁵"), Role::MentionBadge), "{name}");
        }
    }

    #[test]
    fn a_click_on_the_label_closes_the_chat_and_shows_the_room_list() {
        let mut app = App::demo();
        let shown = terminal(&mut app, 50, 20);
        assert_eq!(open_name(&app), Some("rust"));
        app.on_event(click(column(&shown, "Rooms")));
        assert!(app.chat_closed());
        assert_eq!(app.focus, Pane::Sidebar);
        // The room list is in the chat's place, and the bar is gone with the chat.
        let screen = format!("{}", terminal(&mut app, 50, 20).backend());
        assert!(screen.contains("1 Rooms"), "{screen}");
        assert!(!screen.contains("☰ Rooms"), "{screen}");
        // The room that was left is under the cursor.
        assert_eq!(app.cursor_room(), Some(0));
    }

    #[test]
    fn a_click_on_a_room_of_the_bar_opens_it() {
        let mut app = App::demo();
        let shown = terminal(&mut app, 50, 20);
        // The demo's rooms with a count, in the order of its list.
        let line = top(&mut app, 50, 20, 1);
        assert!(line.contains("☰ Rooms B ² D ⁵ O ¹²"), "{line}");
        // The tile, the count and the gap behind it are all the room's.
        let design = column(&shown, "D ");
        for x in [design, design + 1, design + 2, design + 3] {
            let mut app = App::demo();
            let _ = terminal(&mut app, 50, 20);
            app.on_event(click(x));
            assert_eq!(open_name(&app), Some("design"), "{x}");
            assert_eq!(app.focus, Pane::Composer);
            assert!(!app.chat_closed());
            // Open, it is out of the bar; the room that was open is not in it either.
            let line = top(&mut app, 50, 20, 1);
            assert!(line.contains("☰ Rooms B ² O ¹²"), "{line}");
            assert!(!line.contains('⁵'), "{line}");
        }
    }

    #[test]
    fn a_click_on_the_count_of_what_does_not_fit_shows_the_room_list() {
        let rooms = (1..=30).map(|n| room(&format!("room {n}"), n)).collect();
        let mut app = app_with(rooms);
        let shown = terminal(&mut app, 40, 12);
        let more = column(&shown, "(");
        let screen = format!("{}", shown.backend());
        let line = screen.lines().next().unwrap_or_default().to_owned();
        let hidden: usize = line
            .split('(')
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_default();
        // The number is that of the rooms the bar does not show.
        let mut tiles: Vec<usize> = (0..40)
            .filter_map(|x| match app.hits.at(x, 0) {
                Some(HitId::BarRoom(index)) => Some(index),
                _ => None,
            })
            .collect();
        tiles.dedup();
        assert!(hidden > 0 && !tiles.is_empty(), "{line}");
        assert_eq!(hidden + tiles.len(), 30, "{line}");
        // From its bracket to its arrow it is one target.
        let end = column(&shown, ")>") + 1;
        for x in [more, more + 1, end] {
            let mut app = app_with((1..=30).map(|n| room(&format!("room {n}"), n)).collect());
            let _ = terminal(&mut app, 40, 12);
            app.on_event(click(x));
            assert!(app.chat_closed(), "{x}");
            assert_eq!(app.focus, Pane::Sidebar, "{x}");
        }
        // Behind it there is nothing to hit.
        for x in end + 1..40 {
            assert!(!matches!(app.hits.at(x, 0), Some(HitId::Do(_))), "{x}");
        }
    }

    #[test]
    fn a_right_click_on_a_room_of_the_bar_opens_the_rooms_menu() {
        let mut app = App::demo();
        let shown = terminal(&mut app, 50, 20);
        app.on_event(press(MouseButton::Right, column(&shown, "D "), 0));
        // The menu of the room under the pointer, not of the open one.
        let design = app.rooms.iter().find(|room| room.name == "design");
        let design = design.map(|room| room.id.clone()).unwrap_or_default();
        match &app.modal {
            Some(crate::modal::Modal::Menu(menu)) => match &menu.target {
                crate::menu::Target::Room { id, .. } => assert_eq!(*id, design),
                _ => panic!("the menu is not a room's"),
            },
            _ => panic!("no menu"),
        }
        assert_ne!(app.room().map(|room| room.id.clone()), Some(design));
        // The chat stays as it is.
        assert_eq!(open_name(&app), Some("rust"));
    }

    #[test]
    fn the_pointer_over_the_bar_marks_what_a_click_would_take() {
        let mut app = App::demo();
        let shown = terminal(&mut app, 50, 20);
        let (label, design) = (column(&shown, "Rooms"), column(&shown, "D "));
        let hover = app.theme.style(Role::Hover);
        let moved = |column: u16| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.on_event(moved(design));
        let over_room = terminal(&mut app, 50, 20);
        let bg = |terminal: &Terminal<TestBackend>, x: u16| terminal.backend().buffer()[(x, 0)].bg;
        assert_eq!(Some(bg(&over_room, design + 2)), hover.bg);
        assert_ne!(Some(bg(&over_room, label)), hover.bg);
        app.on_event(moved(label));
        let over_label = terminal(&mut app, 50, 20);
        assert_eq!(Some(bg(&over_label, label)), hover.bg);
        assert_ne!(Some(bg(&over_label, design + 2)), hover.bg);
    }

    #[test]
    fn the_bar_is_shown_only_while_the_room_list_is_out_of_sight() {
        let has_bar =
            |app: &mut App, width: u16, height: u16| top(app, width, height, 1).contains(ICON);
        // Wide, with the room list: none.
        let mut app = App::demo();
        assert!(!has_bar(&mut app, 100, 30));
        // Put away, or narrow: there it is.
        app.perform(Action::ToggleSidebar);
        assert!(has_bar(&mut app, 100, 30));
        app.perform(Action::ToggleSidebar);
        assert!(!has_bar(&mut app, 100, 30));
        assert!(has_bar(&mut app, 50, 20));
        // The room list in the chat's place: none.
        app.perform(Action::Focus(Pane::Sidebar));
        assert!(!has_bar(&mut app, 50, 20));
        app.perform(Action::Focus(Pane::Composer));
        assert!(has_bar(&mut app, 50, 20));
        // Too low a window keeps its lines for the messages: three of them stay
        // (under the bar: the frame of the messages, the composer of three lines,
        // the status line).
        assert!(!has_bar(&mut app, 50, 9));
        assert!(has_bar(&mut app, 50, 10));
        // A composer grown by what is written in it counts as high as it is.
        app.composer.insert("one\ntwo\nthree");
        assert!(!has_bar(&mut app, 50, 11));
        assert!(has_bar(&mut app, 50, 12));
        app.composer.set_text("");
        assert!(has_bar(&mut app, 50, 10));
        // So does the line that says what is answered.
        app.perform(Action::SelectOlder);
        app.perform(Action::Reply);
        assert!(!has_bar(&mut app, 50, 10));
        assert!(has_bar(&mut app, 50, 11));
        app.perform(Action::Back);
        assert!(has_bar(&mut app, 50, 10));
        // No chat open: the room list is all there is.
        app.perform(Action::CloseChat);
        assert!(!has_bar(&mut app, 50, 20));
    }

    #[test]
    fn with_the_setting_off_there_is_no_bar_and_the_chat_has_the_line() {
        let mut app = App::demo();
        let with = top(&mut app, 50, 20, 2);
        app.perform(Action::Setting(crate::modal::Setting::RoomsBar));
        let without = top(&mut app, 50, 20, 2);
        assert!(with.contains(ICON) && !without.contains(ICON), "{without}");
        // The chat's own first line moved up into the bar's place.
        assert_eq!(with.lines().nth(1), without.lines().next());
        assert!(!matches!(
            app.hits.at(2, 0),
            Some(HitId::Do(Action::CloseChat) | HitId::BarRoom(_))
        ));
    }

    #[test]
    fn the_bar_coming_and_going_leaves_the_messages_where_they_are() {
        // The screen without its last line (the status line says what was set) and
        // without the scroll bar's thumb, which is as long as the view is high.
        let rows = |app: &mut App| -> Vec<String> {
            let screen = format!("{}", terminal(app, 50, 20).backend());
            let lines: Vec<String> = screen.lines().map(|line| line.replace('┃', "│")).collect();
            lines[..lines.len() - 1].to_vec()
        };
        for scroll in [0, 3] {
            let mut app = App::demo();
            app.timeline.scroll = scroll;
            let with = rows(&mut app);
            assert_eq!(app.timeline.scroll, scroll);
            app.perform(Action::Setting(crate::modal::Setting::RoomsBar));
            let without = rows(&mut app);
            assert_eq!(app.timeline.scroll, scroll);
            assert!(with[0].contains(ICON) && !without[0].contains(ICON));
            // Counted from the newest message, the view keeps its lower end: every
            // line under the bar and the frame's edge stands where it stood, and
            // the line the bar gave back shows one message line more at the top.
            assert_eq!(with[2..], without[2..], "{scroll}");
            app.perform(Action::Setting(crate::modal::Setting::RoomsBar));
            assert_eq!(rows(&mut app), with, "{scroll}");
            assert_eq!(app.timeline.scroll, scroll);
        }
    }

    #[test]
    fn under_a_popup_the_bar_takes_no_clicks_and_tiny_windows_do_not_panic() {
        let mut app = App::demo();
        app.perform(Action::OpenSettings);
        let _ = terminal(&mut app, 50, 20);
        assert!(!matches!(
            app.hits.at(2, 0),
            Some(HitId::Do(Action::CloseChat) | HitId::BarRoom(_))
        ));
        let mut app = app_with((1..=40).map(|n| room(&format!("r{n}"), n * 7)).collect());
        for (width, height) in [(1, 1), (2, 6), (3, 7), (5, 8), (9, 6), (61, 6), (200, 6)] {
            let _ = terminal(&mut app, width, height);
        }
    }
}
