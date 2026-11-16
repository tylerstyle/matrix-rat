//! A voice or audio message while it loads or plays. Its first row becomes
//! `■ 0:05 / 0:12 · 🎤 note.ogg` (the `■` stops it), and its waveform shows how far it
//! has played: the played part in the theme's primary colour, the rest dimmed. Without
//! a waveform a plain bar of the same width goes into the first row; without a known
//! length only the position is shown. A name so long that the message's first line took
//! several rows goes on over those rows.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::AudioView;
use crate::audio::{self, WAVE_BARS};
use crate::model::{Attachment, MediaKind};
use crate::text::{split_at_width, truncate, width};
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
#[cfg(test)]
pub fn head(
    view: AudioView,
    file: &Attachment,
    indent: &str,
    max_width: usize,
    spinner: &str,
    theme: &Theme,
) -> Line<'static> {
    let at = HeadRow {
        nth: 0,
        of: 1,
        full: max_width,
    };
    head_row(view, file, indent, max_width, spinner, theme, at)
}

/// Which of the rows that the message's first line took, and how wide those before it
/// are. A long file name took several: the name goes on over them.
#[derive(Debug, Clone, Copy)]
pub struct HeadRow {
    pub nth: usize,
    pub of: usize,
    /// Columns of each row before this one.
    pub full: usize,
}

/// Row `at.nth` of the first line, `max_width` columns wide, starting with `indent`.
/// `spinner`: the loading indicator's current frame.
pub fn head_row(
    view: AudioView,
    file: &Attachment,
    indent: &str,
    max_width: usize,
    spinner: &str,
    theme: &Theme,
    at: HeadRow,
) -> Line<'static> {
    let muted = fg(theme, Role::Muted);
    let last = at.nth + 1 >= at.of;
    // The first row decides where the name starts, whichever row is asked for.
    let first_width = if at.nth == 0 { max_width } else { at.full };
    let (mut spans, used) = status(view, file, indent, first_width, spinner, theme);
    let icon = match file.kind {
        MediaKind::Voice => "🎤",
        _ => "🔊",
    };
    let room = first_width.saturating_sub(used);
    // Only where a few letters of it fit: an icon and a cut alone say nothing.
    let on_first = room >= 10;
    let name = if on_first {
        format!(" · {icon} {}", file.name)
    } else {
        format!("{icon} {}", file.name)
    };
    let mut rest = name.as_str();
    if on_first {
        if at.nth == 0 {
            let shown = if last {
                truncate(rest, room)
            } else {
                split_at_width(rest, room).0.to_owned()
            };
            spans.push(Span::styled(shown, muted));
            return Line::from(spans);
        }
        rest = split_at_width(rest, room).1;
    } else if at.nth == 0 {
        return Line::from(spans);
    }
    // The rows between the first and this one, each as wide as a row is.
    let between = at.full.saturating_sub(width(indent));
    for _ in 1..at.nth {
        rest = split_at_width(rest, between).1;
    }
    let here = max_width.saturating_sub(width(indent));
    let shown = if last {
        truncate(rest, here)
    } else {
        split_at_width(rest, here).0.to_owned()
    };
    Line::from(vec![
        Span::raw(indent.to_owned()),
        Span::styled(shown, muted),
    ])
}

/// What the first row says before the name, and the columns it takes.
fn status(
    view: AudioView,
    file: &Attachment,
    indent: &str,
    max_width: usize,
    spinner: &str,
    theme: &Theme,
) -> (Vec<Span<'static>>, usize) {
    let muted = fg(theme, Role::Muted);
    let mark = fg(theme, Role::Link).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::raw(indent.to_owned())];
    let mut used = width(indent);
    let room = |used: usize| max_width.saturating_sub(used);
    if room(used) < 2 {
        // Nothing fits, a name neither.
        return (spans, max_width);
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
    (spans, used)
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
    fn a_long_name_goes_on_over_the_rows_its_line_took() {
        let theme = theme();
        let mut long = file(vec![100; 24]);
        long.name = "a_recording_with_quite_a_long_name.ogg".into();
        let row = |nth, of, width| {
            let at = HeadRow { nth, of, full: 30 };
            head_row(playing(5, Some(12)), &long, "  ", width, "", &theme, at).to_string()
        };
        // Three rows of 30 columns: nothing is cut, nothing said twice.
        assert_eq!(row(0, 3, 30), "  ■ 0:05 / 0:12 · 🎤 a_recordi");
        assert_eq!(row(1, 3, 30), "  ng_with_quite_a_long_name.og");
        assert_eq!(row(2, 3, 30), "  g");
        // Two rows, the last one shorter (a read marker at its end): cut there.
        assert_eq!(row(0, 2, 30), "  ■ 0:05 / 0:12 · 🎤 a_recordi");
        assert_eq!(row(1, 2, 20), "  ng_with_quite_a_l…");
        // One row: as before.
        assert_eq!(row(0, 1, 30), "  ■ 0:05 / 0:12 · 🎤 a_record…");
        // No room for a name in the first row: it starts in the second.
        let at = |nth| HeadRow {
            nth,
            of: 3,
            full: 22,
        };
        let narrow = |nth| head_row(playing(5, Some(12)), &long, "  ", 22, "", &theme, at(nth));
        assert_eq!(narrow(0).to_string(), "  ■ 0:05 / 0:12");
        assert_eq!(narrow(1).to_string(), "  🎤 a_recording_with_");
        assert_eq!(narrow(2).to_string(), "  quite_a_long_name.o…");
        for width in 0..40 {
            for nth in 0..4 {
                let at = HeadRow {
                    nth,
                    of: 4,
                    full: width,
                };
                let shown = head_row(playing(5, Some(12)), &long, "  ", width, "", &theme, at);
                assert!(shown.width() <= width.max(2), "{width}/{nth}: {shown}");
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
