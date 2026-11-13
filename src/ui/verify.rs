//! The device verification popup, and the encryption hint in the status line.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::{HitId, HitMap};
use super::modal::{button, centered, popup, row};
use crate::action::Action;
use crate::app::App;
use crate::matrix::{SasEmoji, VerifyStage, VerifyUpdate};
use crate::modal::Modal;
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};
use crate::verify::{Button, Hint, PopupKind, VerifyPopup, VerifyState, describe};

const WIDTH: u16 = 66;
/// Space between two emoji columns.
const GAP: usize = 2;

/// One row of the popup's body.
enum Row {
    Text(String, Role),
    Blank,
    /// One row of the emoji grid: emoji above, their names below.
    Emoji(Vec<SasEmoji>),
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, popup_state: &mut VerifyPopup) {
    let state = &app.verify;
    let (title, footer, rows) = match popup_state.kind {
        PopupKind::SetUp => set_up_body(state),
        PopupKind::Status => status_body(state),
        PopupKind::Flow => match &state.flow {
            Some(flow) => flow_body(flow),
            None => (
                "Verification".to_owned(),
                "Esc close",
                vec![Row::Text("Nothing to verify.".to_owned(), Role::Muted)],
            ),
        },
    };
    let buttons = popup_state.buttons(state);
    popup_state.focus = popup_state.focus.min(buttons.len().saturating_sub(1));

    // Lay the body out for the inner width first, to know how tall the popup must be.
    let inner_width = WIDTH.min(area.width.saturating_sub(2)).saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let theme = &app.theme;
    for row in rows {
        match row {
            Row::Blank => lines.push(Line::default()),
            Row::Text(text, role) => {
                for part in wrap(&text, inner_width.max(1)) {
                    lines.push(Line::from(Span::styled(part, theme.style(role))));
                }
            }
            Row::Emoji(emoji) => emoji_grid(&mut lines, theme, &emoji, inner_width),
        }
    }
    // Border, top padding, body, gap, buttons, bottom padding.
    let height = lines.len() as u16 + 6;
    let Some(rect) = centered(area, WIDTH, height) else {
        return;
    };
    let inner = popup(frame, app, area, rect, &title, footer);
    let theme = &app.theme;
    for (n, line) in lines.into_iter().enumerate() {
        let at = row(inner, 1 + n as u16);
        // Keep the last rows for the buttons when space is short.
        if at.y + 2 >= inner.bottom() {
            break;
        }
        frame.render_widget(line, at);
    }

    let buttons_row = row(inner, inner.height.saturating_sub(2));
    let mut x = buttons_row.x;
    for (n, &choice) in buttons.iter().enumerate() {
        let label = choice.label();
        let w = (width(label) as u16).min(buttons_row.right().saturating_sub(x));
        let rect = Rect::new(x, buttons_row.y, w, buttons_row.height);
        let action = Action::Verify(choice);
        button(frame, theme, rect, label, n == popup_state.focus);
        if app.hover == Some(HitId::Do(action)) && n != popup_state.focus {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        app.hits.add(rect, HitId::Do(action));
        x += w + 2;
    }
}

fn status_body(state: &VerifyState) -> (String, &'static str, Vec<Row>) {
    let security = state.security.unwrap_or_default();
    let rows = describe(&security)
        .into_iter()
        .map(|(label, value)| Row::Text(format!("{label:<15}{value}"), Role::Text))
        .collect();
    ("Encryption".to_owned(), "Enter or Esc close", rows)
}

fn set_up_body(state: &VerifyState) -> (String, &'static str, Vec<Row>) {
    let mut rows = vec![
        Row::Text(
            "Your account has no cross-signing keys yet. Create them now, on this device?"
                .to_owned(),
            Role::Text,
        ),
        Row::Blank,
        Row::Text(
            "They let your other devices, and the people you talk to, confirm that this \
             device is really yours. They are kept on this device, so set up recovery \
             (in another app such as Element, for now) before you rely on them."
                .to_owned(),
            Role::Muted,
        ),
    ];
    if state.setting_up {
        rows.push(Row::Blank);
        rows.push(Row::Text("Setting up…".to_owned(), Role::Muted));
    }
    (
        "Set up encryption".to_owned(),
        "Tab move · Enter choose · Esc close",
        rows,
    )
}

fn flow_body(flow: &VerifyUpdate) -> (String, &'static str, Vec<Row>) {
    let peer = &flow.peer;
    // How to name the other side in a sentence.
    let other = if flow.own {
        format!("your device “{peer}”")
    } else {
        peer.clone()
    };
    let title = match (flow.own, flow.this_device) {
        (true, true) => "Verify this device".to_owned(),
        (true, false) => "Verify your other device".to_owned(),
        (false, _) => format!("Verify {peer}"),
    };
    let text = |text: String| Row::Text(text, Role::Text);
    let muted = |text: &str| Row::Text(text.to_owned(), Role::Muted);
    let rows = match &flow.stage {
        VerifyStage::Requested => vec![
            text(match (flow.own, flow.this_device) {
                (true, true) => format!("Your other device “{peer}” wants to verify this one."),
                (true, false) => format!("Your device “{peer}” asks this one to verify it."),
                (false, _) => format!("{peer} wants to verify you."),
            }),
            Row::Blank,
            muted("You will both see the same emoji if nobody is listening in."),
        ],
        VerifyStage::Waiting => vec![
            text("Waiting for one of your other devices to accept…".to_owned()),
            Row::Blank,
            muted(
                "Open Element or matrix-rat on a device that is already verified and accept \
                 the request there.",
            ),
        ],
        VerifyStage::Ready => vec![
            text(format!("Waiting for {other} to begin the comparison…")),
            Row::Blank,
            muted("Or begin it from here."),
        ],
        VerifyStage::Starting => vec![text(format!("Setting up the comparison with {other}…"))],
        VerifyStage::Compare { emoji, decimals } => {
            let mut rows = vec![
                text(format!("Compare with what {other} shows:")),
                Row::Blank,
            ];
            if emoji.is_empty() {
                let (a, b, c) = decimals;
                rows.push(text(format!("{a:04}   {b:04}   {c:04}")));
            } else {
                rows.push(Row::Emoji(emoji.clone()));
            }
            rows.push(Row::Blank);
            rows.push(muted("Do they match, in the same order?"));
            rows
        }
        VerifyStage::Confirmed => vec![text(format!("Waiting for {other} to confirm…"))],
        VerifyStage::Done => {
            let (headline, detail) = match (flow.own, flow.this_device) {
                (true, true) => (
                    "✓ This device is now verified.".to_owned(),
                    "Your other device sends it the keys it lacks, so encrypted history \
                     unlocks shortly.",
                ),
                (true, false) => (
                    format!("✓ Your device “{peer}” is now verified."),
                    "It can now read your encrypted messages.",
                ),
                (false, _) => (
                    format!("✓ {peer} is verified."),
                    "Their devices are trusted from now on.",
                ),
            };
            vec![
                Row::Text(headline, Role::StatusOk),
                Row::Blank,
                muted(detail),
            ]
        }
        VerifyStage::Cancelled { reason } => vec![Row::Text(format!("✕ {reason}"), Role::Error)],
    };
    let footer = if flow.stage.is_final() {
        "Enter or Esc close"
    } else if flow.stage == VerifyStage::Requested {
        "Tab move · Enter choose · Esc decline"
    } else {
        "Tab move · Enter choose · Esc cancel"
    };
    (title, footer, rows)
}

/// Seven emoji with their names below, in as many rows as the width needs. Columns are
/// as wide as the longest name; emoji are wide characters, so widths are measured.
fn emoji_grid(lines: &mut Vec<Line<'static>>, theme: &Theme, emoji: &[SasEmoji], room: usize) {
    let column = emoji
        .iter()
        .map(|e| width(&e.name).max(width(&e.symbol)))
        .max()
        .unwrap_or(2)
        + GAP;
    let fit = (room / column).max(1);
    // Balance the rows: 4 + 3 rather than 6 + 1.
    let rows = emoji.len().div_ceil(fit);
    let per_row = emoji.len().div_ceil(rows.max(1));
    for (n, chunk) in emoji.chunks(per_row.max(1)).enumerate() {
        if n > 0 {
            lines.push(Line::default());
        }
        let used = chunk.len() * column - GAP;
        let indent = room.saturating_sub(used) / 2;
        let centred = |text: &str| {
            let w = width(text).min(column);
            let left = (column - GAP).saturating_sub(w) / 2;
            let right = column.saturating_sub(left + w);
            format!(
                "{}{}{}",
                " ".repeat(left),
                truncate(text, column),
                " ".repeat(right)
            )
        };
        let mut symbols = vec![Span::raw(" ".repeat(indent))];
        let mut names = vec![Span::raw(" ".repeat(indent))];
        for e in chunk {
            symbols.push(Span::raw(centred(&e.symbol)));
            names.push(Span::styled(centred(&e.name), theme.style(Role::Muted)));
        }
        lines.push(Line::from(symbols));
        lines.push(Line::from(names));
    }
}

/// Adds the encryption hint to the status line, if there is one: a request waiting
/// behind another popup, an unverified device, or an account without cross-signing.
/// `area` is the free part of the status line.
pub fn status_hint(
    state: &VerifyState,
    modal: &Option<Modal>,
    theme: &Theme,
    hover: Option<HitId>,
    hits: &mut HitMap,
    area: Rect,
    spans: &mut Vec<Span<'static>>,
) {
    let popup_open = matches!(
        modal,
        Some(Modal::Verify(VerifyPopup {
            kind: PopupKind::Flow,
            ..
        }))
    );
    let (mark, mark_role, text, verb, action) = match state.hint(popup_open) {
        None => return,
        Some(Hint::Pending { requested }) => (
            "◆",
            Role::StatusKey,
            if requested {
                "verification request"
            } else {
                "verification running"
            },
            if requested { "answer" } else { "show" },
            Action::Verify(Button::Show),
        ),
        Some(Hint::Unverified) => (
            "⚠",
            Role::Error,
            "unverified device",
            "verify",
            Action::StartVerification,
        ),
        Some(Hint::NotSetUp) => (
            "⚠",
            Role::Muted,
            "encryption not set up",
            "set up",
            Action::StartVerification,
        ),
    };
    let start = area.x + spans.iter().map(|s| width(&s.content) as u16).sum::<u16>();
    let hint = [
        Span::styled(format!("{mark} "), theme.style(mark_role)),
        Span::styled(format!("{text} — "), theme.style(Role::Status)),
        Span::styled(verb.to_owned(), theme.style(Role::StatusKey)),
    ];
    let hint_width = hint.iter().map(|s| width(&s.content) as u16).sum::<u16>();
    let end = (start + hint_width).min(area.right());
    let hovered = hover == Some(HitId::Do(action));
    for mut span in hint {
        if hovered {
            span = span.style(theme.style(Role::Hover));
        }
        spans.push(span);
    }
    spans.push(Span::styled("  ", theme.style(Role::Status)));
    if end > start {
        hits.add(Rect::new(start, area.y, end - start, 1), HitId::Do(action));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::App;
    use crate::matrix::{
        DeviceTrust, Identity, MatrixEvent, RecoveryStatus, SasEmoji, Security, VerifyStage,
        VerifyUpdate,
    };

    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn update(stage: VerifyStage) -> MatrixEvent {
        MatrixEvent::Verification(VerifyUpdate {
            flow: 1,
            peer: "Element Desktop (QWERTYUIOP)".into(),
            own: true,
            this_device: true,
            stage,
        })
    }

    #[test]
    fn verification_popups() {
        let mut app = App::demo();
        app.on_matrix(MatrixEvent::Security(Security {
            device: DeviceTrust::Unverified,
            identity: Identity::Present,
            other_devices: true,
            recovery: RecoveryStatus::NeedsKey,
            ..Security::default()
        }));
        insta::assert_snapshot!("verify_status_hint", render(&mut app, 100, 8));
        app.on_matrix(update(VerifyStage::Requested));
        insta::assert_snapshot!("verify_requested", render(&mut app, 100, 24));
        // From the real table: the widest names, and emoji with a variation selector.
        let emoji = [
            ("🐶", "Dog"),
            ("☂️", "Umbrella"),
            ("🎧", "Headphones"),
            ("🦋", "Butterfly"),
            ("☎️", "Telephone"),
            ("🔒", "Lock"),
            ("📌", "Pin"),
        ];
        app.on_matrix(update(VerifyStage::Compare {
            emoji: emoji
                .iter()
                .map(|(symbol, name)| SasEmoji {
                    symbol: (*symbol).into(),
                    name: (*name).into(),
                })
                .collect(),
            decimals: (1, 2, 3),
        }));
        insta::assert_snapshot!("verify_compare", render(&mut app, 100, 24));
        insta::assert_snapshot!("verify_compare_narrow", render(&mut app, 44, 24));
        app.on_matrix(update(VerifyStage::Done));
        insta::assert_snapshot!("verify_done", render(&mut app, 100, 24));
    }
}
