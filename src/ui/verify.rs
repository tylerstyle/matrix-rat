//! The device verification popup, and the encryption hint in the status line.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::{HitId, HitMap};
use super::modal::{button, centered, field, popup, row};
use crate::action::Action;
use crate::app::App;
use crate::matrix::{SasEmoji, VerifyStage, VerifyUpdate};
use crate::modal::Modal;
use crate::text::{width, wrap};
use crate::theme::{Role, Theme};
use crate::verify::{Button, Hint, PopupKind, VerifyPopup, VerifyState, describe, stuck};

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

/// Lays the rows out for `room` columns. `false` if an emoji name had to be cut.
fn layout(rows: Vec<Row>, theme: &Theme, room: usize) -> (Vec<Line<'static>>, bool) {
    let mut lines = Vec::new();
    let mut complete = true;
    for row in rows {
        match row {
            Row::Blank => lines.push(Line::default()),
            Row::Text(text, role) => {
                for part in wrap(&text, room.max(1)) {
                    lines.push(Line::from(Span::styled(part, theme.style(role))));
                }
            }
            Row::Emoji(emoji) => complete &= emoji_grid(&mut lines, theme, &emoji, room),
        }
    }
    (lines, complete)
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, popup_state: &mut VerifyPopup) {
    let state = &app.verify;
    let (title, footer, rows) = match popup_state.kind {
        PopupKind::SetUp => set_up_body(state, popup_state),
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
    let comparing = popup_state.kind == PopupKind::Flow
        && matches!(
            state.flow.as_ref().map(|f| &f.stage),
            Some(VerifyStage::Compare { .. })
        );
    let has_field = popup_state.has_field(state);

    // Lay the body out for the inner width first, to know how tall the popup must be:
    // border, top padding, body, (blank and password field), gap, buttons, padding.
    let inner_width = WIDTH.min(area.width.saturating_sub(2)).saturating_sub(4) as usize;
    let theme = &app.theme;
    let (mut lines, complete) = layout(rows, theme, inner_width);
    let height = |lines: &[Line]| lines.len() as u16 + 6 + if has_field { 2 } else { 0 };
    let mut rect = centered(area, WIDTH, height(&lines));
    // Everything there is to compare must be on screen before "They match" is offered.
    let shown = comparing && complete && rect.is_some_and(|rect| rect.height == height(&lines));
    if comparing && !shown {
        (lines, _) = layout(
            vec![Row::Text(
                "The window is too small to show all seven emoji. Make it larger to \
                 compare them, or cancel."
                    .to_owned(),
                Role::Error,
            )],
            theme,
            inner_width,
        );
        rect = centered(area, WIDTH, height(&lines));
    }
    if shown && !popup_state.compare_shown {
        // The buttons change from Cancel to the answers: a moment before they count.
        popup_state.rearm();
    }
    popup_state.compare_shown = shown;
    let Some(rect) = rect else {
        return;
    };
    let buttons = popup_state.buttons(state);
    let focused = popup_state.focused(state);
    let inner = popup(frame, app, area, rect, &title, footer);
    let theme = &app.theme;
    let body_end = lines.len() as u16;
    for (n, line) in lines.into_iter().enumerate() {
        let at = row(inner, 1 + n as u16);
        // Keep the last rows for the buttons when space is short.
        if at.y + 2 >= inner.bottom() {
            break;
        }
        frame.render_widget(line, at);
    }
    if has_field {
        let at = row(inner, 2 + body_end);
        let masked = "•".repeat(
            popup_state
                .password
                .as_ref()
                .map_or(0, |p| p.chars().count()),
        );
        let cursor = field(
            frame,
            theme,
            at,
            "Password",
            &masked,
            popup_state.field_focused,
        );
        if let Some(cursor) = cursor {
            frame.set_cursor_position(cursor);
        }
    }

    let buttons_row = row(inner, inner.height.saturating_sub(2));
    let mut x = buttons_row.x;
    for &choice in buttons {
        let label = choice.label();
        let w = (width(label) as u16).min(buttons_row.right().saturating_sub(x));
        let rect = Rect::new(x, buttons_row.y, w, buttons_row.height);
        let action = Action::Verify(choice);
        let has_focus = focused == Some(choice);
        button(frame, theme, rect, label, has_focus);
        if app.hover == Some(HitId::Do(action)) && !has_focus {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        app.hits.add(rect, HitId::Do(action));
        x += w + 2;
    }
}

fn status_body(state: &VerifyState) -> (String, &'static str, Vec<Row>) {
    let security = state.security.unwrap_or_default();
    let mut rows: Vec<Row> = describe(&security)
        .into_iter()
        .map(|(label, value)| Row::Text(format!("{label:<15}{value}"), Role::Text))
        .collect();
    if stuck(&security) {
        rows.push(Row::Blank);
        rows.push(Row::Text(
            "No other device of yours can vouch for this one, and there is no recovery \
             key to enter. Log in on a device that is still verified and verify from \
             there, or reset your encryption identity in another app such as Element \
             (matrix-rat cannot do that yet)."
                .to_owned(),
            Role::Muted,
        ));
    }
    ("Encryption".to_owned(), "Enter or Esc close", rows)
}

fn set_up_body(state: &VerifyState, popup: &VerifyPopup) -> (String, &'static str, Vec<Row>) {
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
    } else if popup.password.is_some() {
        rows.push(Row::Blank);
        rows.push(Row::Text(
            "Your server asks for your account password to do this.".to_owned(),
            Role::Text,
        ));
        if let Some(error) = popup.password_error {
            rows.push(Row::Text(error.to_owned(), Role::Error));
        }
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
                 the request there. A device that has just logged in can take a minute to \
                 become known to the others: if nothing shows up there, ask again.",
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

/// The emoji without its variation selector (U+FE0F). Terminals disagree on how wide
/// "☁️" and its kind are, which can shift or swallow what follows on the row; without
/// the selector every terminal agrees (one column, drawn as a symbol). The names below
/// say what each one is either way.
fn plain_symbol(symbol: &str) -> String {
    symbol.replace('\u{fe0f}', "")
}

/// Seven emoji with their names below, in as many rows as the width needs. Columns are
/// as wide as the longest name; emoji are wide characters, so widths are measured.
/// Returns `false` if even one column is wider than `room` (names would be cut).
fn emoji_grid(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    emoji: &[SasEmoji],
    room: usize,
) -> bool {
    let column = emoji
        .iter()
        .map(|e| width(&e.name).max(width(&plain_symbol(&e.symbol))))
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
            let w = width(text);
            let left = (column - GAP).saturating_sub(w) / 2;
            let right = column.saturating_sub(left + w);
            format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
        };
        let mut symbols = vec![Span::raw(" ".repeat(indent))];
        let mut names = vec![Span::raw(" ".repeat(indent))];
        for e in chunk {
            symbols.push(Span::raw(centred(&plain_symbol(&e.symbol))));
            names.push(Span::styled(centred(&e.name), theme.style(Role::Muted)));
        }
        lines.push(Line::from(symbols));
        lines.push(Line::from(names));
    }
    column - GAP <= room
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
    // Mark, its colour, what is wrong, what a click does, and a short form of both.
    let (mark, mark_role, text, verb, short, action) = match state.hint(popup_open) {
        None => return,
        Some(Hint::Pending { requested: true }) => (
            "◆",
            Role::StatusKey,
            "verification request",
            "answer",
            "answer request",
            Action::Verify(Button::Show),
        ),
        Some(Hint::Pending { requested: false }) => (
            "◆",
            Role::StatusKey,
            "verification running",
            "show",
            "verification",
            Action::Verify(Button::Show),
        ),
        Some(Hint::Unverified) => (
            "⚠",
            Role::Error,
            "unverified device",
            "verify",
            "verify device",
            Action::StartVerification,
        ),
        Some(Hint::NotSetUp) => (
            "⚠",
            Role::Muted,
            "encryption not set up",
            "set up",
            "set up encryption",
            Action::StartVerification,
        ),
    };
    let start = area.x + spans.iter().map(|s| width(&s.content) as u16).sum::<u16>();
    let mut hint = vec![
        Span::styled(format!("{mark} "), theme.style(mark_role)),
        Span::styled(format!("{text} — "), theme.style(Role::Status)),
        Span::styled(verb.to_owned(), theme.style(Role::StatusKey)),
    ];
    let measure = |hint: &[Span]| hint.iter().map(|s| width(&s.content) as u16).sum::<u16>();
    // Leave room for at least one key hint after it; otherwise say it shorter.
    if start + measure(&hint) + 20 > area.right() {
        hint.truncate(1);
        hint.push(Span::styled(short.to_owned(), theme.style(Role::StatusKey)));
    }
    let end = (start + measure(&hint)).min(area.right());
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
        app.user_id = Some("@me:example.org".into());
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
        // Too short to show all seven: no answers offered, only Cancel.
        insta::assert_snapshot!("verify_compare_too_small", render(&mut app, 100, 14));
        app.on_matrix(update(VerifyStage::Done));
        insta::assert_snapshot!("verify_done", render(&mut app, 100, 24));
    }

    #[test]
    fn set_up_password_field() {
        use crate::matrix::SetUpOutcome;
        use crate::modal::Modal;
        use crate::verify::VerifyPopup;
        let mut app = App::demo();
        app.user_id = Some("@me:example.org".into());
        app.modal = Some(Modal::Verify(VerifyPopup::set_up()));
        app.verify.setting_up = true;
        app.on_matrix(MatrixEvent::CrossSigningSetUp(
            SetUpOutcome::NeedsPassword { rejected: true },
        ));
        if let Some(Modal::Verify(popup)) = &mut app.modal {
            popup.password = Some("secret".into());
        }
        let screen = render(&mut app, 100, 24);
        assert!(!screen.contains("secret"));
        insta::assert_snapshot!("verify_set_up_password", screen);
    }
}
