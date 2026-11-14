//! Drawing the global message search popup.

use std::ops::Range;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::hit::{HitId, InputTarget};
use super::modal::{centered, popup, row};
use crate::app::App;
use crate::app::global_search::GlobalSearch;
use crate::matrix::search::first_term_match_span;
use crate::text::{truncate, width};
use crate::theme::Role;

const WIDTH: u16 = 78;
const HEIGHT: u16 = 22;
const DEVICE_HINT: &str = "Searches messages loaded on this device";

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, state: &mut GlobalSearch) {
    let Some(rect) = centered(area, WIDTH, HEIGHT) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Search all rooms",
        "↑↓ move · Enter jump · Esc close",
    );
    let theme = &app.theme;
    if inner.width == 0 || inner.height < 2 {
        return;
    }

    // Query row.
    let search = row(inner, 0);
    let max_query = (search.width as usize).saturating_sub(2);
    let view = state.query.view(max_query.saturating_sub(1), false);
    let search_line = if state.query.is_empty() {
        Line::from(vec![
            Span::styled("⌕ ", theme.style(Role::StatusKey)),
            Span::styled(
                truncate("type words to search…", max_query),
                theme.style(Role::Placeholder),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("⌕ ", theme.style(Role::StatusKey)),
            Span::raw(view.text),
        ])
    };
    frame.render_widget(search_line, search);
    frame.set_cursor_position(Position::new(
        (search.x + 2 + view.cursor_col as u16).min(search.right().saturating_sub(1)),
        search.y,
    ));
    app.hits.add(
        Rect::new(search.x + 2, search.y, search.width.saturating_sub(2), 1),
        HitId::Input(InputTarget::GlobalSearchQuery),
    );

    // Separator rule + live match count / status.
    let list_y = if inner.height >= 4 {
        let rule_rect = row(inner, 1);
        let status = status_label(state);
        let status_w = width(&status);
        if !status.is_empty() && rule_rect.width as usize >= status_w + 6 {
            let line_w = rule_rect.width as usize - status_w;
            frame.render_widget(
                Line::from(vec![
                    Span::styled("─".repeat(line_w), theme.style(Role::Border)),
                    Span::styled(status, theme.style(Role::Muted)),
                ]),
                rule_rect,
            );
        } else {
            frame.render_widget(
                Line::from(Span::styled(
                    "─".repeat(rule_rect.width as usize),
                    theme.style(Role::Border),
                )),
                rule_rect,
            );
        }
        2
    } else {
        1
    };

    let avail_rows = inner.height.saturating_sub(list_y) as usize;
    if avail_rows == 0 {
        return;
    }

    let show_note = state.note.is_some() && !state.hits.is_empty() && avail_rows >= 3;
    let list_rows = avail_rows - usize::from(show_note);
    let card_rows: usize = if list_rows >= 2 { 2 } else { 1 };
    let cap = (list_rows / card_rows).max(1);
    state.rows = cap;

    if !state.hits.is_empty() {
        state.selected = state.selected.min(state.hits.len() - 1);
        if state.reveal {
            state.reveal = false;
            if state.selected < state.scroll {
                state.scroll = state.selected;
            } else if state.selected >= state.scroll + cap {
                state.scroll = state.selected + 1 - cap;
            }
        }
        state.scroll = state.scroll.min(state.hits.len().saturating_sub(cap));
    } else {
        state.selected = 0;
        state.scroll = 0;
    }

    if state.hits.is_empty() {
        let msg = if state.query.trim().is_empty() {
            DEVICE_HINT
        } else if state.pending_at.is_some() || state.searching {
            "searching…"
        } else if let Some(note) = &state.note {
            note.as_str()
        } else {
            "no matches"
        };
        let line_w = (inner.width as usize).saturating_sub(2);
        frame.render_widget(
            Line::from(Span::styled(
                format!("  {}", truncate(msg, line_w)),
                theme.style(Role::Muted),
            )),
            row(inner, list_y),
        );
        return;
    }

    let query = state.active_query.clone();
    for (offset, (vis, hit)) in state
        .hits
        .iter()
        .enumerate()
        .skip(state.scroll)
        .take(cap)
        .enumerate()
    {
        let top_y = list_y + (offset * card_rows) as u16;
        let selected = vis == state.selected;
        let hovered = app.hover == Some(HitId::GlobalSearch(vis));
        let card_rect = Rect::new(inner.x, top_y, inner.width, card_rows as u16);
        let head_rect = row(inner, top_y);

        // Header line: marker + room name + sender + right-aligned date.
        let marker = if selected { "› " } else { "  " };
        let marker_role = if selected {
            Role::TitleFocused
        } else {
            Role::Muted
        };
        let head_avail = (head_rect.width as usize).saturating_sub(2);
        let date_str = if head_avail >= 18 && !hit.date.is_empty() {
            truncate(&hit.date, head_avail / 3)
        } else {
            String::new()
        };
        let date_w = width(&date_str);
        let meta_cap = if date_w > 0 {
            head_avail.saturating_sub(date_w + 2)
        } else {
            head_avail
        };
        let (room_label, sender_label) = split_header(&hit.room_name, &hit.sender, meta_cap);
        let used_meta = width(&room_label)
            + if sender_label.is_empty() {
                0
            } else {
                3 + width(&sender_label)
            };
        let gap = if date_w > 0 {
            head_avail.saturating_sub(used_meta + date_w)
        } else {
            0
        };

        let mut head_spans = vec![
            Span::styled(marker, theme.style(marker_role)),
            Span::styled(
                room_label,
                theme.style(if selected {
                    Role::TitleFocused
                } else {
                    Role::SidebarSection
                }),
            ),
        ];
        if !sender_label.is_empty() {
            head_spans.push(Span::styled(" · ", theme.style(Role::Muted)));
            head_spans.push(Span::styled(
                sender_label,
                theme.style(Role::Text).add_modifier(Modifier::BOLD),
            ));
        }
        if date_w > 0 {
            head_spans.push(Span::raw(" ".repeat(gap)));
            head_spans.push(Span::styled(date_str, theme.style(Role::Muted)));
        }
        frame.render_widget(Line::from(head_spans), head_rect);

        if card_rows == 2 {
            let snip_rect = row(inner, top_y + 1);
            let snip_avail = (snip_rect.width as usize).saturating_sub(2);
            let (window, highlight) = snippet_window(&hit.snippet, &query, snip_avail);
            let mut snip_spans = vec![Span::raw("  ")];
            push_highlighted_snippet(&mut snip_spans, &window, highlight, theme);
            frame.render_widget(Line::from(snip_spans), snip_rect);
        }

        if selected {
            frame
                .buffer_mut()
                .set_style(card_rect, theme.style(Role::MessageSelected));
        } else if hovered {
            frame
                .buffer_mut()
                .set_style(card_rect, theme.style(Role::Hover));
        }
        app.hits.add(card_rect, HitId::GlobalSearch(vis));
    }

    if show_note && let Some(note) = &state.note {
        let note_rect = row(inner, (list_y as usize + list_rows) as u16);
        let note_w = (note_rect.width as usize).saturating_sub(2);
        frame.render_widget(
            Line::from(Span::styled(
                format!("  {}", truncate(note, note_w)),
                theme.style(Role::Muted),
            )),
            note_rect,
        );
    }
}

fn status_label(state: &GlobalSearch) -> String {
    if state.pending_at.is_some() || state.searching {
        if state.hits.is_empty() {
            " searching…".to_owned()
        } else {
            format!(" {}+ searching…", state.hits.len())
        }
    } else if !state.hits.is_empty() {
        let noun = if state.hits.len() == 1 {
            "match"
        } else {
            "matches"
        };
        format!(" {} {noun}", state.hits.len())
    } else if !state.active_query.is_empty() {
        " 0 matches".to_owned()
    } else {
        String::new()
    }
}

fn split_header(room: &str, sender: &str, cap: usize) -> (String, String) {
    if cap == 0 {
        return (String::new(), String::new());
    }
    if sender.is_empty() || cap < 8 {
        return (truncate(room, cap), String::new());
    }
    let sep = 3; // " · "
    let room_w = width(room);
    let sender_w = width(sender);
    if room_w + sep + sender_w <= cap {
        return (room.to_owned(), sender.to_owned());
    }
    let room_cap = (cap / 2).max(4).min(cap.saturating_sub(sep + 3));
    let shown_room = truncate(room, room_cap);
    let rem = cap.saturating_sub(width(&shown_room) + sep);
    (shown_room, truncate(sender, rem))
}

fn push_highlighted_snippet<'a>(
    spans: &mut Vec<Span<'a>>,
    window: &str,
    highlight: Option<Range<usize>>,
    theme: &crate::theme::Theme,
) {
    let base = theme.style(Role::Text);
    let Some(range) = highlight.filter(|r| r.start < r.end && r.end <= window.len()) else {
        spans.push(Span::styled(window.to_owned(), base));
        return;
    };
    if !window.is_char_boundary(range.start) || !window.is_char_boundary(range.end) {
        spans.push(Span::styled(window.to_owned(), base));
        return;
    }
    if range.start > 0 {
        spans.push(Span::styled(window[..range.start].to_owned(), base));
    }
    spans.push(Span::styled(
        window[range.clone()].to_owned(),
        theme.style(Role::StatusKey).add_modifier(Modifier::BOLD),
    ));
    if range.end < window.len() {
        spans.push(Span::styled(window[range.end..].to_owned(), base));
    }
}

/// Crops `text` to at most `max_cols` terminal cells centered around the first matched
/// query term when possible, returning the cropped string and highlight byte range.
pub(crate) fn snippet_window(
    text: &str,
    query: &str,
    max_cols: usize,
) -> (String, Option<Range<usize>>) {
    if max_cols == 0 || text.is_empty() {
        return (String::new(), None);
    }
    let total_w = width(text);
    let match_span = first_term_match_span(text, query);
    if total_w <= max_cols {
        return (text.to_owned(), match_span.map(|(start, end)| start..end));
    }
    if max_cols <= 2 {
        return (truncate(text, max_cols), None);
    }

    let pieces: Vec<(usize, &str, usize)> = text
        .grapheme_indices(true)
        .map(|(byte, g)| (byte, g, width(g)))
        .collect();
    let total_g = pieces.len();

    let (start_g, end_g, orig_match) = match match_span {
        Some((ms, me)) => {
            let m_start_g = pieces
                .partition_point(|&(byte, _, _)| byte < ms)
                .min(total_g.saturating_sub(1));
            let m_end_g = pieces
                .partition_point(|&(byte, _, _)| byte < me)
                .max(m_start_g + 1);

            // Center the matched term in `max_cols`, reserving room for leading/trailing ellipsis.
            let budget = max_cols.saturating_sub(2);
            let mut left_col = 0usize;
            for &(_, _, gw) in &pieces[..m_start_g] {
                left_col += gw;
            }
            let match_w: usize = pieces[m_start_g..m_end_g].iter().map(|p| p.2).sum();
            let target_left = left_col.saturating_sub(budget.saturating_sub(match_w) / 2);

            let mut acc = 0usize;
            let mut sg = 0usize;
            while sg < m_start_g && acc < target_left {
                acc += pieces[sg].2;
                sg += 1;
            }
            // Pull back if end hits total so we still fill the full line width.
            let prefix_ell = usize::from(sg > 0);
            let mut used = prefix_ell;
            let mut eg = sg;
            while eg < total_g {
                let trail_reserve = usize::from(eg + 1 < total_g);
                if used + pieces[eg].2 + trail_reserve > max_cols {
                    break;
                }
                used += pieces[eg].2;
                eg += 1;
            }
            while sg > 0 && eg == total_g {
                let prev_w = pieces[sg - 1].2;
                if used + prev_w <= max_cols {
                    sg -= 1;
                    used += prev_w;
                } else {
                    break;
                }
            }
            // Guarantee the match start stays visible when it fits.
            if sg > m_start_g {
                sg = m_start_g;
            }
            (sg, eg.max(sg), Some((ms, me)))
        }
        None => {
            let shown = truncate(text, max_cols);
            return (shown, None);
        }
    };

    let start_byte = pieces.get(start_g).map_or(0, |p| p.0);
    let end_byte = pieces.get(end_g).map_or(text.len(), |p| p.0);
    let mut out = String::new();
    if start_g > 0 {
        out.push('…');
    }
    let offset = out.len();
    out.push_str(&text[start_byte..end_byte]);
    if end_g < total_g {
        out.push('…');
    }

    let highlight = orig_match.and_then(|(ms, me)| {
        let hs = ms.max(start_byte);
        let he = me.min(end_byte);
        (hs < he).then(|| (offset + hs - start_byte)..(offset + he - start_byte))
    });
    (out, highlight)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_window_keeps_late_match_visible_and_valid() {
        let text = format!("{}alpha beta target_word gamma", "prefix ".repeat(30));
        let (win, span) = snippet_window(&text, "target_word", 40);
        assert!(win.contains("target_word"), "{win}");
        assert!(width(&win) <= 40, "width {} > 40: {win}", width(&win));
        let range = span.expect("highlight span");
        assert_eq!(&win[range], "target_word");
    }

    #[test]
    fn snippet_window_handles_emoji_and_wide_chars() {
        let text = "🦊 hello こんにちは world 🚀".repeat(4);
        let (win, span) = snippet_window(&text, "world", 22);
        assert!(win.contains("world"), "{win}");
        assert!(width(&win) <= 22, "width {} > 22", width(&win));
        assert_eq!(&win[span.unwrap()], "world");
    }
}
