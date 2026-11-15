//! Drawing the message details popup: the facts, or the source.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::HitId;
use super::modal::{centered, popup};
use crate::app::App;
use crate::details::{DetailsPopup, DetailsView, Tone};
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};

/// Room for the labels in front of the facts.
const LABEL: usize = 10;

enum Entry {
    Text(Line<'static>),
    /// The buttons under the facts.
    Buttons,
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, details: &mut DetailsPopup) {
    let footer = match details.view {
        DetailsView::Facts => "↑↓ scroll · v source · c copy ID · Esc close",
        DetailsView::Source => "↑↓ scroll · v details · c copy JSON · Esc back",
    };
    let theme = app.theme.clone();
    // Inside the border and its padding.
    let inner_width = 76u16.min(area.width.saturating_sub(2)).saturating_sub(4) as usize;
    let entries = match details.view {
        DetailsView::Facts => facts(&theme, details, inner_width),
        DetailsView::Source => source(&theme, details, inner_width),
    };
    // As high as what is shown needs, as far as the screen allows; the rest scrolls.
    let height = u16::try_from(entries.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let Some(rect) = centered(area, 76, height.min(area.height.saturating_sub(2))) else {
        return;
    };
    let page = (rect.height.saturating_sub(2) as usize).max(1);
    details.page = page;
    details.scroll = details.scroll.min(entries.len().saturating_sub(page));
    let title = match details.view {
        DetailsView::Facts => "Message details".to_owned(),
        DetailsView::Source if entries.len() > page => format!(
            "Message source · {}–{} of {}",
            details.scroll + 1,
            (details.scroll + page).min(entries.len()),
            entries.len()
        ),
        DetailsView::Source => "Message source".to_owned(),
    };
    let inner = popup(frame, app, area, rect, &title, footer);
    if inner.width < 10 || inner.height == 0 {
        return;
    }
    for (n, entry) in entries
        .into_iter()
        .skip(details.scroll)
        .take(inner.height as usize)
        .enumerate()
    {
        let row = Rect::new(inner.x, inner.y + n as u16, inner.width, 1);
        match entry {
            Entry::Text(line) => frame.render_widget(line, row),
            Entry::Buttons => buttons(frame, app, &theme, details, row),
        }
    }
}

/// The facts, each value wrapped beside its label, then the buttons.
fn facts(theme: &Theme, details: &DetailsPopup, room: usize) -> Vec<Entry> {
    let value_room = room.saturating_sub(LABEL).max(8);
    let mut out = Vec::new();
    for fact in details.facts() {
        let role = role(fact.tone);
        for (n, part) in wrap(&fact.value, value_room).into_iter().enumerate() {
            let label = if n == 0 { fact.label } else { "" };
            out.push(Entry::Text(Line::from(vec![
                Span::styled(format!("{label:<LABEL$}"), theme.style(Role::Muted)),
                Span::styled(part, theme.style(role)),
            ])));
        }
    }
    out.push(Entry::Text(Line::default()));
    out.push(Entry::Buttons);
    out
}

/// The source, wrapped to the width (kept between draws: it can be long).
fn source(theme: &Theme, details: &mut DetailsPopup, room: usize) -> Vec<Entry> {
    let room = room.max(8);
    if details.wrapped.as_ref().is_none_or(|(at, _)| *at != room) {
        let wrapped = details
            .source_lines()
            .into_iter()
            .flat_map(|(line, tone)| wrap(&line, room).into_iter().map(move |part| (part, tone)))
            .collect();
        details.wrapped = Some((room, wrapped));
    }
    let mut out: Vec<Entry> = details
        .wrapped
        .as_ref()
        .map(|(_, lines)| {
            lines
                .iter()
                .map(|(line, tone)| {
                    Entry::Text(Line::from(Span::styled(
                        line.clone(),
                        theme.style(role(*tone)),
                    )))
                })
                .collect()
        })
        .unwrap_or_default();
    out.push(Entry::Text(Line::default()));
    out.push(Entry::Buttons);
    out
}

fn role(tone: Tone) -> Role {
    match tone {
        Tone::Normal => Role::Text,
        Tone::Muted => Role::Muted,
        Tone::Warning => Role::Error,
    }
}

/// "View source" (or back) and "Copy …", as clickable pills.
fn buttons(frame: &mut Frame, app: &mut App, theme: &Theme, details: &DetailsPopup, row: Rect) {
    let (flip, copy) = match details.view {
        DetailsView::Facts => (" View source ", " Copy event ID "),
        DetailsView::Source => (" Back to details ", " Copy JSON "),
    };
    let mut x = row.x;
    for (label, hit) in [(flip, HitId::DetailsFlip), (copy, HitId::DetailsCopy)] {
        let w = (width(label) as u16).min(row.right().saturating_sub(x));
        if w == 0 {
            break;
        }
        let rect = Rect::new(x, row.y, w, 1);
        let style = if app.hover == Some(hit) {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Pill)
        };
        frame.render_widget(
            Line::from(Span::styled(truncate(label, w as usize), style)),
            rect,
        );
        app.hits.add(rect, hit);
        x = x.saturating_add(w + 2);
    }
}
