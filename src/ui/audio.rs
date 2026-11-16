//! A voice or audio message while it loads or plays. Its first row becomes
//! `■ 0:05 / 0:12 · 🎤 note.ogg` (the `■` stops it), and its waveform shows how far it
//! has played: the played part in the theme's primary colour, the rest dimmed. Without
//! a waveform a plain bar of the same width goes into the first row; without a known
//! length only the position is shown.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::AudioView;
use crate::audio::{self, WAVE_BARS};
use crate::model::{Attachment, MediaKind};
use crate::text::{truncate, width};
use crate::theme::{Role, Theme};

/// The colour of a role, without its background (the row's selection shows through).
fn fg(theme: &Theme, role: Role) -> Style {
    Style {
        fg: theme.style(role).fg,
        ..Style::new()
    }
}

/// What has played: bold, and in the primary colour.
fn played_style(theme: &Theme) -> Style {
    fg(theme, Role::BorderFocused).add_modifier(Modifier::BOLD)
}

/// The first row, `max_width` columns wide, starting with `indent`. `spinner`: the
/// loading indicator's current frame.
pub fn head(
    view: AudioView,
    file: &Attachment,
    indent: &str,
    max_width: usize,
    spinner: &str,
    theme: &Theme,
) -> Line<'static> {
    let muted = fg(theme, Role::Muted);
    let mark = fg(theme, Role::Link).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::raw(indent.to_owned())];
    let mut used = width(indent);
    let room = |used: usize| max_width.saturating_sub(used);
    if room(used) < 2 {
        return Line::from(spans);
    }
    spans.push(Span::styled("■ ", mark));
    used += 2;
    let status = match view {
        AudioView::Loading => format!("{spinner} loading").trim_start().to_owned(),
        AudioView::Playing {
            position,
            length: Some(length),
            ..
        } => format!("{} / {}", audio::clock(position), audio::length(length)),
        AudioView::Playing { position, .. } => audio::clock(position),
    };
    let status = truncate(&status, room(used));
    used += width(&status);
    spans.push(Span::styled(status, fg(theme, Role::Text)));
    if let AudioView::Playing { paused: true, .. } = view
        && room(used) >= " paused".len()
    {
        spans.push(Span::styled(" paused", muted));
        used += " paused".len();
    }
    // Nothing else to show how far it is: a bar as wide as a waveform would be.
    let waveless = file
        .audio
        .as_ref()
        .is_none_or(|meta| meta.waveform.is_empty());
    if let AudioView::Playing {
        position,
        length: Some(length),
        ..
    } = view
        && waveless
        && room(used) >= 5
    {
        let cells = WAVE_BARS.min(room(used) - 1);
        let done = audio::played(position, length, cells);
        spans.push(Span::raw(" "));
        spans.push(Span::styled("━".repeat(done), played_style(theme)));
        spans.push(Span::styled("─".repeat(cells - done), muted));
        used += 1 + cells;
    }
    let icon = match file.kind {
        MediaKind::Voice => "🎤",
        _ => "🔊",
    };
    let name = format!(" · {icon} {}", file.name);
    // Only where a few letters of it fit: an icon and a cut alone say nothing.
    if room(used) >= 10 {
        spans.push(Span::styled(truncate(&name, room(used)), muted));
    }
    Line::from(spans)
}

/// The waveform's row: the bars it has at rest, those played in the primary colour.
/// `None` where the row stays as it is: while loading, and with no length to measure
/// the position against.
pub fn wave(
    view: AudioView,
    waveform: &[u16],
    indent: &str,
    max_width: usize,
    theme: &Theme,
) -> Option<Line<'static>> {
    let AudioView::Playing {
        position,
        length: Some(length),
        ..
    } = view
    else {
        return None;
    };
    if waveform.is_empty() {
        return None;
    }
    let bars: Vec<char> = audio::bars(waveform).chars().collect();
    let shown = bars.len().min(max_width.saturating_sub(width(indent)));
    let done = audio::played(position, length, shown);
    Some(Line::from(vec![
        Span::raw(indent.to_owned()),
        Span::styled(bars[..done].iter().collect::<String>(), played_style(theme)),
        Span::styled(
            bars[done..shown].iter().collect::<String>(),
            fg(theme, Role::Muted),
        ),
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AudioMeta;
    use matrix_sdk::ruma::events::room::MediaSource;
    use std::sync::Arc;
    use std::time::Duration;

    fn file(waveform: Vec<u16>) -> Attachment {
        Attachment {
            key: "mxc://example.org/v".into(),
            name: "note.ogg".into(),
            size: Some(2000),
            kind: MediaKind::Voice,
            source: Box::new(MediaSource::Plain("mxc://example.org/v".into())),
            audio: Some(Arc::new(AudioMeta {
                duration: Some(Duration::from_secs(12)),
                waveform,
                mime: None,
            })),
        }
    }

    fn playing(secs: u64, length: Option<u64>) -> AudioView {
        AudioView::Playing {
            position: Duration::from_secs(secs),
            length: length.map(Duration::from_secs),
            paused: false,
        }
    }

    fn theme() -> Theme {
        Theme::builtin(crate::theme::DEFAULT_THEME).unwrap()
    }

    /// The text of the spans that are drawn as played (the stop mark aside).
    fn played_part(line: &Line, theme: &Theme) -> String {
        line.spans
            .iter()
            .filter(|span| span.style == played_style(theme) && !span.content.contains('■'))
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn the_first_row_says_where_it_is() {
        let theme = theme();
        let wave = file(vec![100, 900, 500]);
        let line = |view, width| head(view, &wave, "  ", width, "⠋", &theme).to_string();
        assert_eq!(
            line(playing(5, Some(12)), 60),
            "  ■ 0:05 / 0:12 · 🎤 note.ogg"
        );
        assert_eq!(line(playing(5, None), 60), "  ■ 0:05 · 🎤 note.ogg");
        assert_eq!(
            line(
                AudioView::Playing {
                    position: Duration::from_secs(65),
                    length: Some(Duration::from_millis(3_599_600)),
                    paused: true
                },
                60
            ),
            "  ■ 1:05 / 1:00:00 paused · 🎤 note.ogg"
        );
        assert_eq!(line(AudioView::Loading, 60), "  ■ ⠋ loading · 🎤 note.ogg");
        // Narrow: the name goes first, then the rest is cut; never wider than given.
        assert_eq!(line(playing(5, Some(12)), 20), "  ■ 0:05 / 0:12");
        assert_eq!(line(playing(5, Some(12)), 24), "  ■ 0:05 / 0:12");
        assert_eq!(line(playing(5, Some(12)), 25), "  ■ 0:05 / 0:12 · 🎤 not…");
        for width in 0..40 {
            for view in [playing(5, Some(12)), playing(5, None), AudioView::Loading] {
                let shown = head(view, &wave, "  ", width, "⠋", &theme);
                assert!(shown.width() <= width.max(2), "{width}: {shown}");
            }
        }
    }

    #[test]
    fn without_a_waveform_a_plain_bar_shows_how_far() {
        let theme = theme();
        let plain = file(Vec::new());
        let line = head(playing(6, Some(12)), &plain, "  ", 80, "", &theme);
        assert_eq!(
            line.to_string(),
            format!(
                "  ■ 0:06 / 0:12 {}{} · 🎤 note.ogg",
                "━".repeat(12),
                "─".repeat(12)
            )
        );
        assert_eq!(played_part(&line, &theme), "━".repeat(12));
        // Without a length there is nothing to measure against.
        let line = head(playing(6, None), &plain, "  ", 80, "", &theme);
        assert_eq!(line.to_string(), "  ■ 0:06 · 🎤 note.ogg");
        // Narrower: a shorter bar, and no name where it does not fit.
        let line = head(playing(6, Some(12)), &plain, "  ", 30, "", &theme);
        assert_eq!(
            line.to_string(),
            format!("  ■ 0:06 / 0:12 {}{}", "━".repeat(7), "─".repeat(7))
        );
        // With a waveform, the bar is the waveform's row.
        let line = head(
            playing(6, Some(12)),
            &file(vec![1, 2]),
            "  ",
            80,
            "",
            &theme,
        );
        assert!(!line.to_string().contains('━'));
    }

    #[test]
    fn the_waveform_is_split_where_it_is() {
        let theme = theme();
        let waveform: Vec<u16> = (0..12).map(|n| n * 90).collect();
        let bars = audio::bars(&waveform);
        assert_eq!(bars.chars().count(), 12);
        let at =
            |secs, width| wave(playing(secs, Some(12)), &waveform, "  ", width, &theme).unwrap();
        for (secs, played) in [(0, 0), (1, 1), (6, 6), (11, 11), (12, 12), (40, 12)] {
            let line = at(secs, 80);
            assert_eq!(line.to_string(), format!("  {bars}"), "{secs}");
            assert_eq!(
                played_part(&line, &theme),
                bars.chars().take(played).collect::<String>(),
                "{secs}"
            );
        }
        // A narrow pane: as many bars as fit, split in the same proportion.
        let line = at(6, 8);
        assert_eq!(
            line.to_string(),
            format!("  {}", bars.chars().take(6).collect::<String>())
        );
        assert_eq!(played_part(&line, &theme).chars().count(), 3);
        assert_eq!(at(6, 1).to_string(), "  ");
        // Long waveforms keep their 24 bars.
        let long: Vec<u16> = (0..200).map(|n| n * 5).collect();
        let line = wave(playing(3, Some(12)), &long, "  ", 80, &theme).unwrap();
        assert_eq!(played_part(&line, &theme).chars().count(), 6);
        assert_eq!(line.width(), 2 + WAVE_BARS);
        // Left as it is: while loading, without a length, without a waveform.
        assert!(wave(AudioView::Loading, &waveform, "  ", 80, &theme).is_none());
        assert!(wave(playing(3, None), &waveform, "  ", 80, &theme).is_none());
        assert!(wave(playing(3, Some(12)), &[], "  ", 80, &theme).is_none());
        // The rest is dimmed, whatever the theme.
        let line = at(6, 80);
        assert_eq!(line.spans[2].style.fg, theme.style(Role::Muted).fg);
        assert_eq!(line.spans[1].style.fg, theme.style(Role::BorderFocused).fg);
    }
}
