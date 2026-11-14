//! The sessions popup and the key backup popup.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};

use super::hit::{HitId, InputTarget};
use super::modal::{button, centered, field, popup, row};
use crate::action::Action;
use crate::app::App;
use crate::devices::{
    Click, ExportForm, ImportForm, KeysPopup, KeysStage, STATUS_BUTTONS, SessionStage,
    SessionsPopup, Tone, backup_lines, label, last_seen, trust_text, wants_recovery_key, warning,
};
use crate::lineedit::LineEdit;
use crate::matrix::{Session, SessionTrust};
use crate::text::{sanitize_line, truncate, width, wrap};
use crate::theme::Role;

const LIST_WIDTH: u16 = 90;
const DIALOG_WIDTH: u16 = 70;
/// Columns for "no encryption keys", the longest of the trust words.
const TRUST_COLUMN: usize = 18;
const DATE_COLUMN: usize = 10;

/// One text field of a dialog.
struct FieldSpec<'a> {
    label: &'static str,
    value: &'a mut LineEdit,
    masked: bool,
    focused: bool,
    slot: usize,
}

/// A popup made of some text, fields, a row of buttons and a note below them.
struct Dialog<'a> {
    title: &'a str,
    footer: &'a str,
    body: Vec<(String, Role)>,
    fields: Vec<FieldSpec<'a>>,
    /// Label, what it does, and whether it has the keyboard focus.
    buttons: Vec<(&'static str, Click, bool)>,
    note: Option<(String, Role)>,
}

fn draw_dialog(frame: &mut Frame, app: &mut App, area: Rect, dialog: Dialog) {
    let inner_width = DIALOG_WIDTH
        .min(area.width.saturating_sub(2))
        .saturating_sub(4) as usize;
    let theme = app.theme.clone();
    let mut lines: Vec<Line> = Vec::new();
    for (text, role) in &dialog.body {
        if text.is_empty() {
            lines.push(Line::default());
            continue;
        }
        for part in wrap(text, inner_width.max(1)) {
            lines.push(Line::from(Span::styled(part, theme.style(*role))));
        }
    }
    // A note often quotes what the server said.
    let note: Vec<Line> = dialog
        .note
        .iter()
        .flat_map(|(text, role)| {
            wrap(&sanitize_line(text), inner_width.max(1))
                .into_iter()
                .map(|part| Line::from(Span::styled(part, theme.style(*role))))
        })
        .collect();
    let fields = dialog.fields.len() as u16;
    let height = 3
        + lines.len() as u16
        + if fields > 0 { fields + 1 } else { 0 }
        + if dialog.buttons.is_empty() { 0 } else { 2 }
        + if note.is_empty() {
            0
        } else {
            note.len() as u16 + 1
        };
    // Never so flat that it would not be drawn at all (a lone "Asking the server…").
    let Some(rect) = centered(area, DIALOG_WIDTH, height.max(6)) else {
        return;
    };
    let inner = popup(frame, app, area, rect, dialog.title, dialog.footer);
    let mut y = 1u16;
    for line in lines {
        frame.render_widget(line, row(inner, y));
        y += 1;
    }
    let mut cursor = None;
    if fields > 0 {
        y += 1;
        for spec in dialog.fields {
            let (at, value_rect) = field(
                frame,
                &theme,
                row(inner, y),
                spec.label,
                spec.value,
                spec.masked,
                spec.focused,
            );
            app.hits
                .add(value_rect, HitId::Input(InputTarget::Devices(spec.slot)));
            cursor = cursor.or(at);
            y += 1;
        }
    }
    if !dialog.buttons.is_empty() {
        y += 1;
        let at = row(inner, y);
        let mut x = at.x;
        for (text, click, focused) in dialog.buttons {
            let w = (width(text) as u16).min(at.right().saturating_sub(x));
            let rect = Rect::new(x, at.y, w, at.height);
            button(frame, &theme, rect, text, focused);
            app.hits.add(rect, HitId::Do(Action::Devices(click)));
            x += w + 2;
        }
        y += 1;
    }
    y += 1;
    for line in note {
        frame.render_widget(line, row(inner, y));
        y += 1;
    }
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

pub fn draw_sessions(frame: &mut Frame, app: &mut App, area: Rect, state: &mut SessionsPopup) {
    match &mut state.stage {
        SessionStage::List => sessions_list(frame, app, area, state),
        SessionStage::Rename(rename) => {
            let busy = rename.request.is_some();
            let body = vec![(
                "The name is shown to you and to the people you talk to. It proves \
                 nothing: anyone with access to the account can change it."
                    .to_owned(),
                Role::Muted,
            )];
            let note = if busy {
                Some(("Renaming…".to_owned(), Role::Muted))
            } else {
                rename.error.clone().map(|error| (error, Role::Error))
            };
            let dialog = Dialog {
                title: "Rename session",
                footer: "Enter rename · Esc back",
                body,
                fields: if busy {
                    Vec::new()
                } else {
                    vec![FieldSpec {
                        label: "Name",
                        value: &mut rename.name,
                        masked: false,
                        focused: true,
                        slot: 0,
                    }]
                },
                buttons: if busy {
                    Vec::new()
                } else {
                    vec![
                        (" Rename ", Click::Confirm, true),
                        (" Cancel ", Click::Cancel, false),
                    ]
                },
                note,
            };
            draw_dialog(frame, app, area, dialog);
        }
        SessionStage::SignOut(sign_out) => {
            let name = sign_out.label.clone();
            let busy = sign_out.request.is_some();
            let mut body = Vec::new();
            let mut buttons = Vec::new();
            let mut footer = "Tab choose · Enter · Esc back";
            match &sign_out.web {
                _ if busy => body.push(("Asking the server…".to_owned(), Role::Muted)),
                Some(Some(url)) => {
                    body.push((
                        format!(
                            "This server takes no password for this: the sessions of your \
                             account are managed on its web pages. Sign out {name} there:"
                        ),
                        Role::Text,
                    ));
                    body.push((String::new(), Role::Text));
                    body.push((sanitize_line(url), Role::StatusKey));
                    buttons.push((" Open in browser ", Click::OpenWeb, sign_out.on_confirm));
                    buttons.push((" Back ", Click::Cancel, !sign_out.on_confirm));
                }
                Some(None) => {
                    body.push((
                        format!(
                            "This server takes no password for signing a session out \
                             (single sign-on, or an account service), and it names no web \
                             page for it. Nothing was changed. Sign out {name} from your \
                             account's page on the server's website, or from another \
                             client such as Element (Settings → Sessions)."
                        ),
                        Role::Text,
                    ));
                    buttons.push((" Back ", Click::Cancel, true));
                    footer = "Enter · Esc back";
                }
                None if sign_out.password.is_some() => {
                    body.push((
                        format!("The server asks for your account password to sign out {name}."),
                        Role::Text,
                    ));
                    buttons.push((" Sign out ", Click::Confirm, true));
                    buttons.push((" Cancel ", Click::Cancel, false));
                    footer = "Enter sign out · Esc back";
                }
                None => {
                    body.push((format!("Sign out {name}?"), Role::Text));
                    body.push((String::new(), Role::Text));
                    body.push((
                        "That session is logged out at once and has to log in again. \
                         Encrypted messages whose keys only it had (not backed up, not \
                         on another session) can no longer be read by anyone. This \
                         cannot be undone."
                            .to_owned(),
                        Role::Muted,
                    ));
                    buttons.push((" Sign out ", Click::Confirm, sign_out.on_confirm));
                    buttons.push((" Cancel ", Click::Cancel, !sign_out.on_confirm));
                }
            }
            let typing = sign_out.typing();
            let note = sign_out.error.clone().map(|error| (error, Role::Error));
            let fields = match &mut sign_out.password {
                Some(password) if typing => vec![FieldSpec {
                    label: "Password",
                    value: password,
                    masked: true,
                    focused: true,
                    slot: 0,
                }],
                _ => Vec::new(),
            };
            let dialog = Dialog {
                title: "Sign out a session",
                footer,
                body,
                fields,
                buttons,
                note,
            };
            draw_dialog(frame, app, area, dialog);
        }
    }
}

fn sessions_list(frame: &mut Frame, app: &mut App, area: Rect, state: &mut SessionsPopup) {
    let list = app.devices.list.clone();
    let inner_width = LIST_WIDTH
        .min(area.width.saturating_sub(2))
        .saturating_sub(4) as usize;
    let theme = app.theme.clone();
    // What stands above the list.
    let mut head: Vec<(String, Role)> = Vec::new();
    let sessions: &[Session] = match &list {
        None => {
            head.push(("Loading…".to_owned(), Role::Muted));
            &[]
        }
        Some(Err(error)) => {
            head.push((sanitize_line(error), Role::Error));
            &[]
        }
        Some(Ok(list)) => {
            match warning(list) {
                Some(text) => head.push((text, Role::Error)),
                None => head.push((
                    match list.sessions.len() {
                        1 => "This is the only session, and it is verified.".to_owned(),
                        n => format!("All {n} sessions are verified."),
                    },
                    Role::StatusOk,
                )),
            }
            if !list.vouched {
                head.push((
                    "This device is not verified itself, so it cannot check the others: \
                     “verified” below is what the server says."
                        .to_owned(),
                    Role::Error,
                ));
            }
            &list.sessions
        }
    };
    let head: Vec<Line> = head
        .iter()
        .flat_map(|(text, role)| {
            wrap(text, inner_width.max(1))
                .into_iter()
                .map(|part| Line::from(Span::styled(part, theme.style(*role))))
        })
        .collect();
    let per = if state.show_addresses { 2 } else { 1 };
    let wanted = (sessions.len().clamp(2, 10) * per) as u16;
    let Some(rect) = centered(area, LIST_WIDTH, head.len() as u16 + wanted + 5) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        "Sessions",
        "r rename · v verify · Del sign out · a addresses · Esc close",
    );
    let mut y = 0u16;
    for line in head {
        frame.render_widget(line, row(inner, y));
        y += 1;
    }
    y += 1;
    // The list, with the last two rows kept for the buttons.
    let rows = (inner.height.saturating_sub(y + 2) as usize) / per;
    state.page = rows;
    let selected = state.selected_index(sessions);
    if state.reveal {
        state.reveal = false;
        if selected < state.scroll {
            state.scroll = selected;
        } else if rows > 0 && selected >= state.scroll + rows {
            state.scroll = selected + 1 - rows;
        }
    }
    state.scroll = state.scroll.min(sessions.len().saturating_sub(rows));
    let right = 2 + TRUST_COLUMN + 2 + DATE_COLUMN;
    for (offset, session) in sessions.iter().skip(state.scroll).take(rows).enumerate() {
        let index = state.scroll + offset;
        let at = row(inner, y + (offset * per) as u16);
        if at.width == 0 {
            continue;
        }
        let is_selected = index == selected;
        let click = HitId::Do(Action::Devices(Click::Row(index)));
        let name_style = if is_selected {
            theme.style(Role::TitleFocused)
        } else if app.hover == Some(click) {
            theme.style(Role::Hover)
        } else {
            theme.style(Role::Text)
        };
        let this = if session.current {
            " · this device"
        } else {
            ""
        };
        let name_room = (at.width as usize).saturating_sub(2 + right + width(this));
        let name = truncate(&label(session), name_room);
        let pad = (at.width as usize).saturating_sub(2 + width(&name) + width(this) + right);
        let trust_role = match session.trust {
            SessionTrust::Verified => Role::StatusOk,
            SessionTrust::Unverified | SessionTrust::NoKeys => Role::Error,
        };
        let spans = vec![
            Span::styled(
                if is_selected { "› " } else { "  " },
                theme.style(Role::TitleFocused),
            ),
            Span::styled(name, name_style),
            Span::styled(this, theme.style(Role::StatusOk)),
            Span::raw(" ".repeat(pad + 2)),
            Span::styled(
                format!("{:<TRUST_COLUMN$}", trust_text(session.trust)),
                theme.style(trust_role),
            ),
            Span::raw("  "),
            Span::styled(
                format!("{:>DATE_COLUMN$}", last_seen(session)),
                theme.style(Role::Muted),
            ),
        ];
        frame.render_widget(Line::from(spans), at);
        app.hits.add(at, click);
        if state.show_addresses {
            let below = row(inner, y + (offset * per) as u16 + 1);
            let text = match &session.last_ip {
                Some(ip) => format!("    last seen from {}", sanitize_line(ip)),
                None => "    the server gives no address".to_owned(),
            };
            frame.render_widget(
                Line::from(Span::styled(
                    truncate(&text, below.width as usize),
                    theme.style(Role::Muted),
                )),
                below,
            );
            app.hits.add(below, click);
        }
    }
    if matches!(&list, Some(Ok(list)) if list.sessions.is_empty()) {
        frame.render_widget(
            Line::from(Span::styled(
                "The server lists no sessions.",
                theme.style(Role::Muted),
            )),
            row(inner, y),
        );
    }
    // Buttons for the selected session.
    let at = row(inner, inner.height.saturating_sub(1));
    let mut x = at.x;
    let addresses = if state.show_addresses {
        " Hide addresses "
    } else {
        " Show addresses "
    };
    for (text, click) in [
        (" Rename ", Click::Rename),
        (" Verify ", Click::Verify),
        (" Sign out ", Click::SignOut),
        (addresses, Click::Addresses),
    ] {
        let w = (width(text) as u16).min(at.right().saturating_sub(x));
        let rect = Rect::new(x, at.y, w, at.height);
        let id = HitId::Do(Action::Devices(click));
        button(frame, &theme, rect, text, app.hover == Some(id));
        app.hits.add(rect, id);
        x += w + 2;
    }
}

pub fn draw_keys(frame: &mut Frame, app: &mut App, area: Rect, state: &mut KeysPopup) {
    let focused = state.focused();
    match &mut state.stage {
        KeysStage::Status => {
            let security = app.verify.security;
            let body = backup_lines(security, app.devices.backup)
                .into_iter()
                .map(|(tone, text)| match tone {
                    Tone::Good => (format!("✓ {text}"), Role::StatusOk),
                    Tone::Warn => (format!("! {text}"), Role::Error),
                    Tone::Plain => (format!("  {text}"), Role::Text),
                })
                .collect();
            let labels = [
                " Enter recovery key ",
                " Export keys… ",
                " Import keys… ",
                " Close ",
            ];
            let buttons = STATUS_BUTTONS
                .iter()
                .zip(labels)
                .map(|(click, text)| (text, *click, *click == focused))
                .collect();
            let note = (!wants_recovery_key(security)).then(|| {
                (
                    "The recovery key is only asked for while this device lacks what it \
                     unlocks."
                        .to_owned(),
                    Role::Muted,
                )
            });
            let dialog = Dialog {
                title: "Key backup and recovery",
                footer: "k recovery key · e export · i import · Esc close",
                body,
                fields: Vec::new(),
                buttons,
                note,
            };
            draw_dialog(frame, app, area, dialog);
        }
        KeysStage::Export(form) => {
            let busy = form.request.is_some();
            let focus = form.focus;
            let note = if busy {
                Some((
                    "Exporting… (protecting the file takes a moment)".to_owned(),
                    Role::Muted,
                ))
            } else {
                form.error.clone().map(|error| (error, Role::Error))
            };
            let ExportForm {
                path,
                passphrase,
                again,
                ..
            } = form;
            let fields = if busy {
                Vec::new()
            } else {
                vec![
                    FieldSpec {
                        label: "File",
                        value: path,
                        masked: false,
                        focused: focus == 0,
                        slot: 0,
                    },
                    FieldSpec {
                        label: "Passphrase",
                        value: passphrase,
                        masked: true,
                        focused: focus == 1,
                        slot: 1,
                    },
                    FieldSpec {
                        label: "Once more",
                        value: again,
                        masked: true,
                        focused: focus == 2,
                        slot: 2,
                    },
                ]
            };
            let dialog = Dialog {
                title: "Export message keys",
                footer: "Tab next · Enter · Esc back",
                body: vec![(
                    "Writes every message key this device has into a new file, protected \
                     by a passphrase you choose. Another session (matrix-rat, Element) can \
                     import it to read your encrypted history. So can anyone else who \
                     gets hold of the file and the passphrase."
                        .to_owned(),
                    Role::Muted,
                )],
                fields,
                buttons: if busy {
                    Vec::new()
                } else {
                    vec![
                        (" Export ", Click::Confirm, focus == ExportForm::CONFIRM),
                        (" Cancel ", Click::Cancel, focus == ExportForm::CANCEL),
                    ]
                },
                note,
            };
            draw_dialog(frame, app, area, dialog);
        }
        KeysStage::Import(form) => {
            let busy = form.request.is_some();
            let focus = form.focus;
            let note = if busy {
                Some(("Importing…".to_owned(), Role::Muted))
            } else {
                form.error.clone().map(|error| (error, Role::Error))
            };
            let ImportForm {
                path, passphrase, ..
            } = form;
            let fields = if busy {
                Vec::new()
            } else {
                vec![
                    FieldSpec {
                        label: "File",
                        value: path,
                        masked: false,
                        focused: focus == 0,
                        slot: 0,
                    },
                    FieldSpec {
                        label: "Passphrase",
                        value: passphrase,
                        masked: true,
                        focused: focus == 1,
                        slot: 1,
                    },
                ]
            };
            let dialog = Dialog {
                title: "Import message keys",
                footer: "Tab next · Enter · Esc back",
                body: vec![(
                    "Reads message keys from a file that matrix-rat or Element exported, \
                     so this device can read the messages they belong to. Type where the \
                     file is, or drop it on this window."
                        .to_owned(),
                    Role::Muted,
                )],
                fields,
                buttons: if busy {
                    Vec::new()
                } else {
                    vec![
                        (" Import ", Click::Confirm, focus == ImportForm::CONFIRM),
                        (" Cancel ", Click::Cancel, focus == ImportForm::CANCEL),
                    ]
                },
                note,
            };
            draw_dialog(frame, app, area, dialog);
        }
    }
}
