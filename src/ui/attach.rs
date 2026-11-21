//! The files waiting to be sent, shown above the message text: one line each, with a
//! ✕ to take it off again (and a ▶ / ■ on recorded voice notes to listen before sending).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::hit::{HitId, HitMap};
use crate::app::AudioView;
use crate::images::human_size;
use crate::text::{truncate, width};
use crate::theme::{Role, Theme};
use crate::upload::Chosen;

/// Rows given to waiting files at most; further files are counted in the last one.
const MAX_ROWS: usize = 4;
const PLAY: &str = " ▶ ";
const STOP: &str = " ■ ";
const CLOSE: &str = " ✕ ";

/// Rows needed to show `count` waiting files.
pub fn rows(count: usize) -> u16 {
    count.min(MAX_ROWS) as u16
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame,
    files: &[Chosen],
    state: &crate::app::AttachState,
    audio: &crate::app::AudioState,
    theme: &Theme,
    hits: &mut HitMap,
    area: Rect,
) {
    let available = area.height as usize;
    // With more files than rows, the last row says how many are not shown.
    let listed = if files.len() > available {
        available.saturating_sub(1)
    } else {
        files.len()
    };
    for (index, file) in files.iter().take(listed).enumerate() {
        let row = Rect::new(area.x, area.y + index as u16, area.width, 1);
        let size = format!(" · {}", human_size(file.size));
        if let Some(voice) = state.voice(&file.path) {
            // A voice message recorded here: drop size first, then the label, so
            // `🎤 0:12 ▶ ✕` stays legible and clickable at narrow widths.
            let playing = audio.playing_draft(&file.path);
            let play_sym = if playing.is_some() { STOP } else { PLAY };
            let controls_w = width(play_sym) + width(CLOSE);
            let budget = (row.width as usize).saturating_sub(controls_w);
            let label = match playing {
                Some(AudioView::Playing {
                    position,
                    length,
                    paused,
                }) => {
                    let pos = crate::audio::clock(position);
                    let len = crate::audio::length(length.unwrap_or(voice.duration));
                    let tag = if paused { " paused" } else { "" };
                    let full = format!("🎤 {} · {pos} / {len}{tag}", file.name);
                    let no_name = format!("🎤 {pos} / {len}{tag}");
                    let short = format!("🎤 {pos}");
                    if width(&full) <= budget {
                        full
                    } else if width(&no_name) <= budget {
                        no_name
                    } else {
                        truncate(&short, budget)
                    }
                }
                _ => {
                    let clock = crate::audio::clock(voice.duration);
                    let full = format!("🎤 {} · {clock}{size}", file.name);
                    let no_size = format!("🎤 {} · {clock}", file.name);
                    let clock_only = format!("🎤 {clock}");
                    if width(&full) <= budget {
                        full
                    } else if width(&no_size) <= budget {
                        no_size
                    } else {
                        truncate(&clock_only, budget)
                    }
                }
            };
            let label_width = (width(&label) as u16).min(row.width);
            frame.render_widget(
                Line::from(Span::styled(label, theme.style(Role::Banner))),
                row,
            );
            let play = Rect::new(row.x + label_width, row.y, width(play_sym) as u16, 1);
            if play.right() <= row.right() {
                let id = HitId::AttachmentPlay(index);
                let style = if playing.is_some() {
                    theme.style(Role::Link).add_modifier(Modifier::BOLD)
                } else {
                    theme.style(Role::Link)
                };
                frame.render_widget(Line::from(Span::styled(play_sym, style)), play);
                hits.add(play, id);
            }
            let close = Rect::new(play.right(), row.y, width(CLOSE) as u16, 1);
            if close.right() <= row.right() {
                let id = HitId::Attachment(index);
                frame.render_widget(
                    Line::from(Span::styled(CLOSE, theme.style(Role::Muted))),
                    close,
                );
                hits.add(close, id);
            }
        } else {
            let fixed = 3 + width(&size) + width(CLOSE) + 1;
            let name = truncate(
                &file.name,
                (row.width as usize).saturating_sub(fixed).max(1),
            );
            let label = format!("📎 {name}{size}");
            let label_width = (width(&label) as u16).min(row.width);
            frame.render_widget(
                Line::from(Span::styled(label, theme.style(Role::Banner))),
                row,
            );
            let close = Rect::new(row.x + label_width + 1, row.y, width(CLOSE) as u16, 1);
            if close.right() <= row.right() {
                let id = HitId::Attachment(index);
                frame.render_widget(
                    Line::from(Span::styled(CLOSE, theme.style(Role::Muted))),
                    close,
                );
                hits.add(close, id);
            }
        }
    }
    if listed < files.len() && available > 0 {
        let row = Rect::new(area.x, area.y + listed as u16, area.width, 1);
        let more = files.len() - listed;
        let label = match files.first() {
            // A single row for several files: the first by name, the rest counted.
            Some(first) if listed == 0 => {
                let icon = if state.voice(&first.path).is_some() {
                    "🎤"
                } else {
                    "📎"
                };
                let rest = format!("  +{} more", more - 1);
                let name = truncate(
                    &first.name,
                    (row.width as usize).saturating_sub(3 + width(&rest)).max(1),
                );
                format!("{icon} {name}{rest}")
            }
            _ => format!(
                "📎 … and {more} more file{}",
                if more == 1 { "" } else { "s" }
            ),
        };
        frame.render_widget(
            Line::from(Span::styled(
                truncate(&label, row.width as usize),
                theme.style(Role::Banner),
            )),
            row,
        );
    }
}
