//! Drawing the doctor's popup ("What the app found here"): the report section by
//! section, label and value in columns, what to do in the muted colour under them, and
//! the copy button kept in view below the report, which scrolls.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::hit::HitId;
use super::modal::{centered, popup};
use crate::app::App;
use crate::doctor::{Level, Popup};
use crate::text::{truncate, width, wrap_at_spaces};
use crate::theme::{Role, Theme};

/// The popup's width, where the screen has it.
const WIDTH: u16 = 80;
/// The mark in front of a line ("!" missing, "~" a note), and a space.
const MARK: usize = 2;
/// Room for the longest label ("Key bindings in the config") and a space.
const LABEL: usize = 27;
const TITLE: &str = "What the app found";
const FOOTER: &str = "↑↓ scroll · c copy · Esc close";
const COPY: &str = " Copy as text ";

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, doctor: &mut Popup) {
    let theme = app.theme.clone();
    // Inside the border and its padding.
    let room = WIDTH.min(area.width.saturating_sub(2)).saturating_sub(4) as usize;
    let lines = report_lines(&theme, doctor, room);
    // The report, then a blank line and the buttons, inside the border: as high as that
    // needs, as far as the screen allows. The report scrolls; the buttons stay.
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let Some(rect) = centered(area, WIDTH, height.min(area.height.saturating_sub(2))) else {
        return;
    };
    let page = (rect.height.saturating_sub(4) as usize).max(1);
    doctor.page = page;
    doctor.scroll = doctor.scroll.min(lines.len().saturating_sub(page));
    let title = if lines.len() > page {
        format!(
            "{TITLE} · {}–{} of {}",
            doctor.scroll + 1,
            (doctor.scroll + page).min(lines.len()),
            lines.len()
        )
    } else {
        TITLE.to_owned()
    };
    let inner = popup(frame, app, area, rect, &title, FOOTER);
    if inner.width < 10 || inner.height == 0 {
        return;
    }
    let rows = inner.height.saturating_sub(2) as usize;
    for (n, line) in lines.into_iter().skip(doctor.scroll).take(rows).enumerate() {
        frame.render_widget(line, Rect::new(inner.x, inner.y + n as u16, inner.width, 1));
    }
    let buttons_row = Rect::new(inner.x, inner.bottom() - 1, inner.width, 1);
    buttons(frame, app, &theme, doctor, buttons_row);
}

/// The report as screen lines for `room` columns.
fn report_lines(theme: &Theme, doctor: &Popup, room: usize) -> Vec<Line<'static>> {
    // A narrow screen gives the values at least half of what there is.
    let label = LABEL.min(room.saturating_sub(MARK) / 2).max(1);
    let value_room = room.saturating_sub(MARK + label).max(8);
    let indent = " ".repeat(MARK + label);
    let mut out = Vec::new();
    for (n, section) in doctor.report.sections.iter().enumerate() {
        if n > 0 {
            out.push(Line::default());
        }
        out.push(Line::from(Span::styled(
            section.title,
            theme.style(Role::SidebarSection),
        )));
        for line in &section.lines {
            let (mark, mark_style, value_style) = match line.level {
                Level::Fine => ("  ", theme.style(Role::Text), theme.style(Role::Text)),
                Level::Note => ("~ ", theme.style(Role::StatusKey), theme.style(Role::Text)),
                Level::Missing => (
                    "! ",
                    theme.style(Role::Error).add_modifier(Modifier::BOLD),
                    theme.style(Role::Error),
                ),
            };
            let shown_label = truncate(line.label, label.saturating_sub(1));
            let padding = " ".repeat(label.saturating_sub(width(&shown_label)));
            for (part_at, part) in wrap_at_spaces(&line.value, value_room)
                .into_iter()
                .enumerate()
            {
                let mut spans = Vec::with_capacity(4);
                if part_at == 0 {
                    spans.push(Span::styled(mark, mark_style));
                    spans.push(Span::styled(
                        format!("{shown_label}{padding}"),
                        theme.style(Role::Text),
                    ));
                } else {
                    spans.push(Span::raw(indent.clone()));
                }
                spans.push(Span::styled(part, value_style));
                out.push(Line::from(spans));
            }
            if let Some(hint) = &line.hint {
                for part in wrap_at_spaces(hint, value_room) {
                    out.push(Line::from(vec![
                        Span::raw(indent.clone()),
                        Span::styled(part, theme.style(Role::Muted)),
                    ]));
                }
            }
        }
    }
    out
}

/// "Copy as text" as a clickable pill, and how much is missing.
fn buttons(frame: &mut Frame, app: &mut App, theme: &Theme, doctor: &Popup, row: Rect) {
    let pill = (width(COPY) as u16).min(row.width);
    let rect = Rect::new(row.x, row.y, pill, 1);
    frame.render_widget(
        Line::from(Span::styled(
            truncate(COPY, pill as usize),
            theme.style(Role::Pill),
        )),
        rect,
    );
    app.hits.add(rect, HitId::DoctorCopy);
    let (summary, role) = match doctor.report.missing() {
        0 => ("Nothing is missing".to_owned(), Role::StatusOk),
        1 => ("1 thing is missing".to_owned(), Role::Error),
        n => (format!("{n} things are missing"), Role::Error),
    };
    let x = row.x.saturating_add(pill + 2);
    let left = row.right().saturating_sub(x);
    if left > 0 {
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&summary, left as usize),
                theme.style(role),
            )),
            Rect::new(x, row.y, left, 1),
        );
    }
}
