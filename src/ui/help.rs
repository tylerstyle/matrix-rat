use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use super::hit::HitId;
use crate::app::App;
use crate::keymap::{Context, DEFAULT};
use crate::theme::{Role, Theme};

const MOUSE_HINT: &str =
    "Mouse: click to focus · right click for a menu · wheel scrolls · drag the room list's edge";

/// The heading and the bindings of one context.
fn section(theme: &Theme, context: Context) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        context.title(),
        theme.style(Role::SidebarSection),
    ))];
    for binding in DEFAULT.iter().filter(|b| b.context == context) {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<12}", binding.label),
                theme.style(Role::StatusKey),
            ),
            Span::raw(binding.description),
        ]));
    }
    lines
}

/// Lays the sections out in `columns` columns of about the same length, so that the
/// overlay is as low as it can be. `spaced` puts a blank line between sections. A
/// section that runs on into the next column gets its heading again there, and a
/// heading never ends up alone at the bottom of a column.
fn flow(
    theme: &Theme,
    sections: &[(Context, Vec<Line<'static>>)],
    columns: usize,
    spaced: bool,
) -> Vec<Vec<Line<'static>>> {
    // Every line with what it is ('h' heading, 'b' blank, ' ' binding) and its section.
    let mut flat: Vec<(char, Context, Line<'static>)> = Vec::new();
    for (context, section) in sections {
        if spaced && !flat.is_empty() {
            flat.push(('b', *context, Line::default()));
        }
        for (n, line) in section.iter().enumerate() {
            flat.push((if n == 0 { 'h' } else { ' ' }, *context, line.clone()));
        }
    }
    let columns = columns.max(1);
    let mut out = Vec::new();
    let mut rest = flat.as_slice();
    for column in 0..columns {
        let mut lines: Vec<Line> = Vec::new();
        if let Some((' ', context, _)) = rest.first() {
            lines.push(Line::from(Span::styled(
                format!("{} (continued)", context.title()),
                theme.style(Role::SidebarSection),
            )));
        }
        let mut take = (rest.len() + lines.len()).div_ceil(columns - column) - lines.len();
        take = take.min(rest.len());
        // Not a heading or a blank line as the last thing in a column.
        while take > 0 && take < rest.len() && rest[take - 1].0 != ' ' {
            take -= 1;
        }
        let (now, later) = rest.split_at(take);
        lines.extend(now.iter().map(|(_, _, line)| line.clone()));
        out.push(lines);
        // No blank line at the top of the next column.
        rest = match later.first() {
            Some(('b', _, _)) => &later[1..],
            _ => later,
        };
    }
    out
}

fn sections(theme: &Theme) -> Vec<(Context, Vec<Line<'static>>)> {
    [
        Context::Global,
        Context::Sidebar,
        Context::Timeline,
        Context::Composer,
    ]
    .into_iter()
    .map(|context| (context, section(theme, context)))
    .collect()
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    let sections = sections(&theme);

    // Two columns when there is room, otherwise one long one. Every key has to be
    // reachable: the blank lines go first, then the mouse hint, and what still does
    // not fit can be scrolled to.
    let columns_wanted = if area.width >= 100 { 2 } else { 1 };
    let width = if columns_wanted == 2 {
        96
    } else {
        56.min(area.width.saturating_sub(2))
    };
    // Rows for keys when the status line below stays visible.
    let room = area.height.saturating_sub(3) as usize;
    let rows_of = |columns: &[Vec<Line>]| columns.iter().map(Vec::len).max().unwrap_or(0);
    let mut columns = flow(&theme, &sections, columns_wanted, true);
    if rows_of(&columns) > room {
        columns = flow(&theme, &sections, columns_wanted, false);
    }
    let rows = rows_of(&columns);
    let hint_rows = u16::from(room > rows);
    // Tight: the overlay may cover the status line too, for one more row of keys.
    let most = if rows > room {
        area.height
    } else {
        area.height.saturating_sub(1)
    };
    let height = (rows as u16 + 2 + hint_rows).min(most);
    if width < 4 || height < 3 {
        return;
    }
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (most - height) / 2,
        width,
        height,
    );
    app.hits.add(area, HitId::Backdrop);
    app.hits.add(popup, HitId::Overlay);

    let visible = (height - 2 - hint_rows) as usize;
    let max_scroll = rows.saturating_sub(visible);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let footer = if max_scroll > 0 {
        " ↑↓ scroll · Esc to close "
    } else {
        " Esc to close "
    };

    // Dim everything behind the overlay.
    frame.buffer_mut().set_style(
        area,
        ratatui::style::Style::new().add_modifier(ratatui::style::Modifier::DIM),
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.style(Role::OverlayBorder))
        .style(theme.style(Role::Text))
        .title(Line::from(Span::styled(
            " Keys ",
            theme.style(Role::TitleFocused),
        )))
        .title_bottom(Line::from(Span::styled(footer, theme.style(Role::Muted))).right_aligned());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let inner = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let [keys, hint] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(hint_rows)]).areas(inner);
    let scroll = (app.help_scroll as u16, 0);
    let count = columns.len() as u16;
    for (n, lines) in columns.into_iter().enumerate() {
        let column = keys.width / count.max(1);
        let rect = Rect::new(keys.x + n as u16 * column, keys.y, column, keys.height);
        frame.render_widget(Paragraph::new(lines).scroll(scroll), rect);
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            MOUSE_HINT,
            theme.style(Role::Muted),
        ))),
        hint,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sections_text(columns: usize, spaced: bool) -> Vec<Vec<String>> {
        let theme = Theme::builtin(crate::theme::DEFAULT_THEME).unwrap();
        text(&flow(&theme, &sections(&theme), columns, spaced))
    }

    fn text(columns: &[Vec<Line<'static>>]) -> Vec<Vec<String>> {
        columns
            .iter()
            .map(|lines| lines.iter().map(ToString::to_string).collect())
            .collect()
    }

    #[test]
    fn columns_balance_and_keep_every_key() {
        let bindings = DEFAULT.len();
        for spaced in [true, false] {
            for columns in [1, 2, 3] {
                let flowed = sections_text(columns, spaced);
                assert_eq!(flowed.len(), columns);
                // Nothing is lost, whatever the layout.
                let keys = flowed
                    .iter()
                    .flatten()
                    .filter(|line| line.starts_with(' '))
                    .count();
                assert_eq!(keys, bindings, "{columns} columns, spaced: {spaced}");
                for column in &flowed {
                    // No column starts or ends with a blank line or ends on a heading.
                    let (first, last) = (column.first().unwrap(), column.last().unwrap());
                    assert!(!first.is_empty() && !last.is_empty());
                    assert!(last.starts_with(' '), "{last:?} ends a column");
                }
                if columns == 2 {
                    let (a, b) = (flowed[0].len() as isize, flowed[1].len() as isize);
                    assert!((a - b).abs() <= 2, "{a} and {b} lines");
                    // A section that runs over says whose keys follow.
                    assert!(!flowed[1][0].starts_with(' '));
                }
            }
        }
    }
}
