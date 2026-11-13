//! Drawing the room popups: "New chat or room" (join, direct chat, create, invite) and
//! "Room info" with its member list. Also the two small ways in for the mouse.

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::hit::HitId;
use super::modal::{button, centered, field, popup, row};
use crate::action::Action;
use crate::app::App;
use crate::manage::{
    Click, Confirm, InfoPopup, InfoRow, NewPopup, Stage, notify_label, role_label,
};
use crate::matrix::RoomMember;
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};

/// The `+` at the right end of the room list's title: new chat or room.
pub fn new_button(frame: &mut Frame, app: &mut App, area: Rect) {
    // The title (" 1 Rooms ") keeps its place; the button only where both fit.
    if area.width < 16 || area.height == 0 {
        return;
    }
    let rect = Rect::new(area.right() - 4, area.y, 3, 1);
    let action = Action::NewRoom;
    let role = if app.hover == Some(HitId::Do(action)) {
        Role::Hover
    } else {
        Role::StatusKey
    };
    frame.render_widget(Line::from(Span::styled(" + ", app.theme.style(role))), rect);
    app.hits.add(rect, HitId::Do(action));
}

/// Makes the room's name in the title of the messages pane open the room info.
pub fn title_hit(hits: &mut super::hit::HitMap, area: Rect, title: &str) {
    // " 2 " before the name, one space after it, inside the corner.
    let shown = (width(title) as u16 + 4).min(area.width.saturating_sub(2));
    hits.add(
        Rect::new(area.x + 1, area.y, shown, 1),
        HitId::Do(Action::RoomInfo),
    );
}

/// `text` cut or padded to exactly `columns` on screen.
fn fit(text: &str, columns: usize) -> String {
    let mut out = truncate(text, columns);
    let used = width(&out);
    out.extend(std::iter::repeat_n(' ', columns.saturating_sub(used)));
    out
}

fn hit(click: Click) -> HitId {
    HitId::Do(Action::Manage(click))
}

// ----- new chat or room --------------------------------------------------------------

pub fn draw_new(frame: &mut Frame, app: &mut App, area: Rect, form: &NewPopup) {
    match form.stage {
        Stage::Choose => chooser(frame, app, area, form),
        Stage::Create => create(frame, app, area, form),
        Stage::Join | Stage::Direct | Stage::Invite => one_field(frame, app, area, form),
    }
}

fn chooser(frame: &mut Frame, app: &mut App, area: Rect, form: &NewPopup) {
    let Some(rect) = centered(area, 58, 12) else {
        return;
    };
    let inner = popup(
        frame,
        app,
        area,
        rect,
        Stage::Choose.title(),
        "↑↓ select · Enter · Esc close",
    );
    let theme = app.theme.clone();
    // Three rows each where there is room, otherwise packed.
    let pitch = if inner.height >= 9 { 3 } else { 2 };
    for (n, stage) in Stage::CHOICES.into_iter().enumerate() {
        let selected = form.choice == n;
        let top = 1.min(inner.height.saturating_sub(6)) + n as u16 * pitch;
        let title = row(inner, top);
        let summary = row(inner, top + 1);
        let marker = if selected { "› " } else { "  " };
        let style = if selected {
            theme.style(Role::TitleFocused)
        } else {
            theme.style(Role::Text)
        };
        frame.render_widget(
            Line::from(vec![
                Span::styled(marker, theme.style(Role::TitleFocused)),
                Span::styled(format!("{} ", n + 1), theme.style(Role::StatusKey)),
                Span::styled(stage.title(), style),
            ]),
            title,
        );
        frame.render_widget(
            Line::from(Span::styled(
                format!(
                    "    {}",
                    truncate(stage.summary(), inner.width.saturating_sub(4) as usize)
                ),
                theme.style(Role::Muted),
            )),
            summary,
        );
        for rect in [title, summary] {
            let id = hit(Click::Stage(stage));
            if app.hover == Some(id) && !selected {
                frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
            }
            app.hits.add(rect, id);
        }
    }
}

/// Error, or what is being waited for, under a form.
fn message(frame: &mut Frame, theme: &Theme, form: &NewPopup, inner: Rect, top: u16) {
    let (text, role) = match (&form.busy, &form.error) {
        (Some(busy), _) => ((*busy).to_owned(), Role::Muted),
        (None, Some(error)) => (crate::text::sanitize(error), Role::Error),
        (None, None) => return,
    };
    let top = top.min(inner.height);
    frame.render_widget(
        Paragraph::new(text)
            .style(theme.style(role))
            .wrap(Wrap { trim: false }),
        Rect::new(inner.x, inner.y + top, inner.width, inner.height - top),
    );
}

fn footer(form: &NewPopup) -> &'static str {
    if form.busy.is_some() {
        "Esc close"
    } else if form.single {
        "Enter continue · Esc close"
    } else {
        "Enter continue · Esc back"
    }
}

fn one_field(frame: &mut Frame, app: &mut App, area: Rect, form: &NewPopup) {
    let Some(rect) = centered(area, 66, 11) else {
        return;
    };
    let inner = popup(frame, app, area, rect, form.stage.title(), footer(form));
    let theme = app.theme.clone();
    let busy = form.busy.is_some();
    let (hint, label, value, action) = match form.stage {
        Stage::Join => (
            "An address like #room:server or !id:server, or a matrix.to link pasted as it is."
                .to_owned(),
            "Address",
            &form.address,
            " Join ",
        ),
        Stage::Invite => (
            format!(
                "Who to invite to {}? A user ID looks like @name:server.",
                form.room.as_ref().map_or("this room", |(_, name)| name)
            ),
            "User ID",
            &form.user,
            " Invite ",
        ),
        _ => (
            "The user ID of the person, like @name:server. If you already have a chat with \
             them, it is opened."
                .to_owned(),
            "User ID",
            &form.user,
            " Start chat ",
        ),
    };
    let hint_height = 2.min(inner.height);
    frame.render_widget(
        Paragraph::new(hint)
            .style(theme.style(Role::Muted))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y, inner.width, hint_height),
    );
    let input = row(inner, 3);
    let cursor = field(frame, &theme, input, label, value, !busy);
    app.hits.add(input, hit(Click::Slot(0)));
    let buttons = row(inner, 5);
    if !busy && buttons.width > 0 {
        let w = (width(action) as u16).min(buttons.width);
        let rect = Rect::new(buttons.x, buttons.y, w, 1);
        button(frame, &theme, rect, action, true);
        app.hits.add(rect, hit(Click::Submit));
    }
    message(frame, &theme, form, inner, 7);
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

/// A labelled switch with the same label column as `field`.
fn switch(
    frame: &mut Frame,
    theme: &Theme,
    rect: Rect,
    label: &str,
    value: Vec<Span<'static>>,
    focused: bool,
) {
    if rect.width == 0 {
        return;
    }
    let label_style = if focused {
        theme.style(Role::TitleFocused)
    } else {
        theme.style(Role::Muted)
    };
    let mut spans = vec![Span::styled(format!("{label:<13}"), label_style)];
    spans.extend(value);
    frame.render_widget(Line::from(spans), rect);
}

fn create(frame: &mut Frame, app: &mut App, area: Rect, form: &NewPopup) {
    let Some(rect) = centered(area, 66, 16) else {
        return;
    };
    let inner = popup(frame, app, area, rect, form.stage.title(), footer(form));
    let theme = app.theme.clone();
    let busy = form.busy.is_some();
    let focused = |slot: usize| !busy && form.focus == slot;
    // Every other row when there is room.
    let pitch = if inner.height >= 12 { 2 } else { 1 };
    let at = |slot: usize| row(inner, (pitch - 1) + slot as u16 * pitch);
    let mut cursor = None;

    for (slot, label, value) in [
        (NewPopup::NAME, "Name", &form.name),
        (NewPopup::TOPIC, "Topic", &form.topic),
    ] {
        let rect = at(slot);
        cursor = cursor.or(field(frame, &theme, rect, label, value, focused(slot)));
        app.hits.add(rect, hit(Click::Slot(slot)));
    }

    let arrow = theme.style(Role::StatusKey);
    let access = at(NewPopup::ACCESS);
    let text = if form.public {
        "Public · anyone can join by its address"
    } else {
        "Private · by invitation only"
    };
    switch(
        frame,
        &theme,
        access,
        "Access",
        vec![
            Span::styled("◂ ", arrow),
            Span::styled(
                truncate(text, access.width.saturating_sub(17) as usize),
                theme.style(Role::Text),
            ),
            Span::styled(" ▸", arrow),
        ],
        focused(NewPopup::ACCESS),
    );
    app.hits.add(access, hit(Click::Flip(NewPopup::ACCESS)));

    let extra = at(NewPopup::EXTRA);
    if form.public {
        // Shown with its sigil; the server completes it with its own name.
        let shown = format!("#{}", form.alias.trim_start_matches('#'));
        cursor = cursor.or(field(
            frame,
            &theme,
            extra,
            "Address",
            &shown,
            focused(NewPopup::EXTRA),
        ));
        app.hits.add(extra, hit(Click::Slot(NewPopup::EXTRA)));
    } else {
        let (dot, role) = if form.encrypted {
            ("● on", Role::StatusOk)
        } else {
            (
                "○ off · cannot be switched on by this app later",
                Role::Muted,
            )
        };
        switch(
            frame,
            &theme,
            extra,
            "Encryption",
            vec![Span::styled(
                truncate(dot, extra.width.saturating_sub(13) as usize),
                theme.style(role),
            )],
            focused(NewPopup::EXTRA),
        );
        app.hits.add(extra, hit(Click::Flip(NewPopup::EXTRA)));
    }

    let buttons = at(NewPopup::BUTTON);
    if !busy && buttons.width > 0 {
        let label = " Create room ";
        let w = (width(label) as u16).min(buttons.width);
        let rect = Rect::new(buttons.x, buttons.y, w, 1);
        button(frame, &theme, rect, label, form.focus == NewPopup::BUTTON);
        app.hits.add(rect, hit(Click::Submit));
    }
    message(
        frame,
        &theme,
        form,
        inner,
        (pitch - 1) + (NewPopup::BUTTON as u16 + 1) * pitch,
    );
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

// ----- room info ---------------------------------------------------------------------

const FACT_LABEL: usize = 12;
const ACTION_LABEL: usize = 18;
/// Lines of topic shown before it is cut.
const TOPIC_LINES: usize = 6;

/// One line of the scrolling part of the room info view.
enum Entry {
    Text(Line<'static>),
    /// Index into the popup's rows.
    Row(usize, InfoRow),
    /// The heading of the member list, with the filter.
    Members,
}

fn fact(theme: &Theme, label: &str, value: String, role: Role, room: usize) -> Entry {
    Entry::Text(Line::from(vec![
        Span::styled(format!("{label:<FACT_LABEL$}"), theme.style(Role::Muted)),
        Span::styled(
            truncate(&value, room.saturating_sub(FACT_LABEL)),
            theme.style(role),
        ),
    ]))
}

pub fn draw_info(frame: &mut Frame, app: &mut App, area: Rect, info: &mut InfoPopup) {
    let Some(rect) = centered(area, 74, area.height.saturating_sub(2).min(40)) else {
        return;
    };
    let rows = info.rows();
    info.selected = info.selected.min(rows.len().saturating_sub(1));
    let current = rows.get(info.selected).copied();
    let footer = if info.editing.is_some() {
        "Enter save · Esc cancel"
    } else if matches!(current, Some(InfoRow::Member(_))) {
        "↑↓ select · type to filter · Esc close"
    } else {
        "↑↓ select · Enter · Esc close"
    };
    let inner = popup(frame, app, area, rect, "Room info", footer);
    let theme = app.theme.clone();
    if inner.width < 8 || inner.height == 0 {
        return;
    }
    let room = inner.width as usize;

    // What scrolls: the room's description, what can be done, the members.
    let mut entries: Vec<Entry> = Vec::new();
    let name_style = theme.style(Role::TitleFocused).add_modifier(Modifier::BOLD);
    entries.push(Entry::Text(Line::from(Span::styled(
        truncate(&crate::text::sanitize_line(&info.name), room),
        name_style,
    ))));
    match &info.details {
        Some(details) => {
            if !details.topic.is_empty() {
                let mut topic = wrap(&crate::text::sanitize(&details.topic), room);
                if topic.len() > TOPIC_LINES {
                    topic.truncate(TOPIC_LINES);
                    if let Some(last) = topic.last_mut() {
                        *last = format!("{}…", truncate(last, room.saturating_sub(1)));
                    }
                }
                entries.extend(topic.into_iter().map(|line| Entry::Text(Line::from(line))));
            }
            entries.push(Entry::Text(Line::default()));
            if !info.invited() {
                let (alias, role) = match &details.alias {
                    Some(alias) => (alias.clone(), Role::Text),
                    None => ("none".to_owned(), Role::Muted),
                };
                entries.push(fact(&theme, "Address", alias, role, room));
            }
            entries.push(fact(
                &theme,
                "Room ID",
                info.room_id.clone(),
                Role::Text,
                room,
            ));
            if !info.invited() {
                let (text, role) = match details.encrypted {
                    Some(true) => ("on", Role::StatusOk),
                    Some(false) => ("off · messages here are not encrypted", Role::Text),
                    None => ("unknown", Role::Muted),
                };
                entries.push(fact(&theme, "Encryption", text.to_owned(), role, room));
            }
            let members = match (details.joined, details.invited) {
                (joined, 0) => format!("{joined}"),
                (joined, invited) => format!("{joined} · {invited} invited"),
            };
            entries.push(fact(&theme, "Members", members, Role::Text, room));
        }
        None => {
            entries.push(Entry::Text(Line::default()));
            entries.push(fact(
                &theme,
                "Room ID",
                info.room_id.clone(),
                Role::Text,
                room,
            ));
            if info.error.is_none() {
                entries.push(Entry::Text(Line::from(Span::styled(
                    "Loading…",
                    theme.style(Role::Muted),
                ))));
            }
        }
    }
    entries.push(Entry::Text(Line::default()));
    let mut heading_done = false;
    for (index, row) in rows.iter().enumerate() {
        if matches!(row, InfoRow::Member(_)) && !heading_done {
            heading_done = true;
            entries.push(Entry::Text(Line::default()));
            entries.push(Entry::Members);
        }
        entries.push(Entry::Row(index, *row));
    }
    if !heading_done
        && info
            .details
            .as_ref()
            .is_some_and(|d| !d.members.is_empty() || d.members_loading)
    {
        entries.push(Entry::Text(Line::default()));
        entries.push(Entry::Members);
        if !info.filter.is_empty() {
            entries.push(Entry::Text(Line::from(Span::styled(
                "  no member matches",
                theme.style(Role::Muted),
            ))));
        }
    }

    // The last row is kept for what is going on; the rest scrolls.
    let status = inner.height >= 4;
    let page = (inner.height - u16::from(status)) as usize;
    // A view of another size (the window was resized) shows the selection again.
    info.reveal |= info.page != page;
    info.page = page;
    let selected_line = entries
        .iter()
        .position(|entry| matches!(entry, Entry::Row(index, _) if *index == info.selected));
    if std::mem::take(&mut info.reveal)
        && let Some(line) = selected_line
    {
        if info.selected == 0 && line < page {
            // The first row: show the description above it as well.
            info.scroll = 0;
        } else if line < info.scroll + 1 {
            // With the line above it, which may be the members' heading.
            info.scroll = line.saturating_sub(1);
        } else if line >= info.scroll + page {
            info.scroll = line + 1 - page;
        }
    }
    info.scroll = info.scroll.min(entries.len().saturating_sub(page));

    let busy = info.busy.is_some();
    let mut cursor = None;
    for (n, entry) in entries.iter().skip(info.scroll).take(page).enumerate() {
        let rect = Rect::new(inner.x, inner.y + n as u16, inner.width, 1);
        match entry {
            Entry::Text(line) => frame.render_widget(line, rect),
            Entry::Members => {
                let mut spans = vec![
                    Span::styled("Members", theme.style(Role::SidebarSection)),
                    Span::styled("  ⌕ ", theme.style(Role::StatusKey)),
                ];
                if info.filter.is_empty() {
                    spans.push(Span::styled(
                        "type to filter…",
                        theme.style(Role::Placeholder),
                    ));
                } else {
                    spans.push(Span::raw(truncate(&info.filter, room.saturating_sub(14))));
                    if info.editing.is_none() {
                        let x =
                            rect.x + (11 + width(&info.filter)).min(room.saturating_sub(1)) as u16;
                        cursor = Some(Position::new(x, rect.y));
                    }
                }
                if info.details.as_ref().is_some_and(|d| d.members_loading) {
                    spans.push(Span::styled("  loading…", theme.style(Role::Muted)));
                }
                frame.render_widget(Line::from(spans), rect);
            }
            Entry::Row(index, row) => {
                let selected = *index == info.selected;
                let at = info_row(frame, app, &theme, info, rect, *index, *row, selected, busy);
                cursor = at.or(cursor);
            }
        }
    }

    if status {
        let rect = row(inner, inner.height - 1);
        let more = if info.scroll + page < entries.len() {
            " ↓ more"
        } else {
            ""
        };
        let (text, role) = status_text(info, current);
        let space = room.saturating_sub(width(more));
        frame.render_widget(
            Line::from(vec![
                Span::styled(fit(&text, space), theme.style(role)),
                Span::styled(more, theme.style(Role::Muted)),
            ]),
            rect,
        );
    }
    if let Some(cursor) = cursor {
        frame.set_cursor_position(cursor);
    }
}

/// The bottom line: what went wrong, what is under way, what is being asked, or what
/// the selected row is for.
fn status_text(info: &InfoPopup, current: Option<InfoRow>) -> (String, Role) {
    if let Some(error) = &info.error {
        return (crate::text::sanitize_line(error), Role::Error);
    }
    if let Some(busy) = info.busy {
        return (busy.to_owned(), Role::Muted);
    }
    match &info.confirm {
        Some(Confirm::Leave) => return (info.leave_confirm().to_owned(), Role::Error),
        Some(Confirm::Remove(user_id)) => {
            let name = info
                .details
                .iter()
                .flat_map(|details| &details.members)
                .find(|member| member.user_id == *user_id)
                .map_or(user_id.as_str(), |member| member.name.as_str());
            return (
                format!("Remove {name} from the room? Del again to remove, Esc to keep"),
                Role::Error,
            );
        }
        None => {}
    }
    if info.editing.is_some() {
        return ("Enter to save, Esc to cancel".to_owned(), Role::Muted);
    }
    let text = match current {
        Some(InfoRow::Favourite) => "Favourites are listed first in the room list.",
        Some(InfoRow::Notify) => "←→ choose which messages here notify you.",
        Some(InfoRow::Name | InfoRow::Topic) => "Enter to edit.",
        Some(InfoRow::Invite) => "Invite someone by user ID.",
        Some(InfoRow::Leave) if info.invited() => "",
        Some(InfoRow::Leave) => "You need a new invitation or the address to come back.",
        Some(row @ InfoRow::Member(_)) => match info.member(row) {
            Some(member) if member.own => "This is you.",
            Some(member) if member.removable => "Enter: direct chat · Del: remove from the room",
            Some(_) => "Enter: direct chat",
            None => "",
        },
        None => "",
    };
    (text.to_owned(), Role::Muted)
}

/// Draws one selectable row and registers what clicking it does. Returns where the
/// text cursor belongs if the row is being typed into.
#[expect(clippy::too_many_arguments)]
fn info_row(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    info: &InfoPopup,
    rect: Rect,
    index: usize,
    row: InfoRow,
    selected: bool,
    busy: bool,
) -> Option<Position> {
    let marker = Span::styled(
        if selected { "› " } else { "  " },
        theme.style(Role::TitleFocused),
    );
    let label_style = if selected {
        theme.style(Role::TitleFocused)
    } else {
        theme.style(Role::Text)
    };
    let room = (rect.width as usize).saturating_sub(2);
    let hovered = matches!(
        app.hover,
        Some(HitId::Do(Action::Manage(
            Click::Row(i) | Click::Select(i) | Click::Step(i, _)
        ))) if i == index
    );
    let finish = |frame: &mut Frame, spans: Vec<Span<'static>>| {
        frame.render_widget(Line::from(spans), rect);
        if hovered && !selected {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
    };

    if let Some(member) = info.member(row) {
        app.hits.add(rect, hit(Click::Select(index)));
        member_row(
            frame,
            app,
            theme,
            member,
            rect,
            marker,
            label_style,
            selected && !busy,
        );
        if hovered && !selected {
            frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
        }
        return None;
    }

    // Name or topic while it is typed: the whole row is the input.
    if let Some((editing, text)) = &info.editing
        && *editing == row
    {
        let label = if row == InfoRow::Name {
            "Name"
        } else {
            "Topic"
        };
        let space = room.saturating_sub(ACTION_LABEL + 1);
        let mut shown = text.clone();
        while width(&shown) > space && !shown.is_empty() {
            shown.remove(0);
        }
        let x = rect.x + (2 + ACTION_LABEL + width(&shown)).min(rect.width as usize - 1) as u16;
        let rest = "_".repeat(space.saturating_sub(width(&shown)));
        finish(
            frame,
            vec![
                marker,
                Span::styled(format!("{label:<ACTION_LABEL$}"), label_style),
                Span::raw(shown),
                Span::styled(rest, theme.style(Role::Border)),
            ],
        );
        return Some(Position::new(x, rect.y));
    }

    let (label, label_style) = match row {
        InfoRow::Favourite => ("Favourite", label_style),
        InfoRow::Notify => ("Notifications", label_style),
        InfoRow::Name => ("Rename…", label_style),
        InfoRow::Topic => ("Change topic…", label_style),
        InfoRow::Invite => ("Invite someone…", label_style),
        InfoRow::Leave => (info.leave_label(), theme.style(Role::Error)),
        InfoRow::Member(_) => ("", label_style),
    };
    let mut spans = vec![
        marker,
        Span::styled(format!("{label:<ACTION_LABEL$}"), label_style),
    ];
    app.hits.add(rect, hit(Click::Row(index)));
    let value_x = rect.x + 2 + ACTION_LABEL as u16;
    match row {
        InfoRow::Favourite => {
            let on = info
                .details
                .as_ref()
                .map_or(info.kind == crate::model::RoomKind::Favourite, |d| {
                    d.favourite
                });
            let (text, role) = if on {
                ("● on", Role::StatusOk)
            } else {
                ("○ off", Role::Muted)
            };
            spans.push(Span::styled(text, theme.style(role)));
        }
        InfoRow::Notify => {
            let level = info.details.as_ref().and_then(|d| d.notify);
            let text = level.map_or("", notify_label);
            let arrow = theme.style(Role::StatusKey);
            spans.push(Span::styled("◂ ", arrow));
            spans.push(Span::styled(text, theme.style(Role::Text)));
            spans.push(Span::styled(" ▸", arrow));
            if value_x + 4 + (width(text) as u16) <= rect.right() {
                app.hits.add(
                    Rect::new(value_x, rect.y, 2, 1),
                    hit(Click::Step(index, -1)),
                );
                app.hits.add(
                    Rect::new(value_x + 2 + width(text) as u16, rect.y, 2, 1),
                    hit(Click::Step(index, 1)),
                );
            }
        }
        _ => {}
    }
    finish(frame, spans);
    None
}

#[expect(clippy::too_many_arguments)]
fn member_row(
    frame: &mut Frame,
    app: &mut App,
    theme: &Theme,
    member: &RoomMember,
    rect: Rect,
    marker: Span<'static>,
    name_style: ratatui::style::Style,
    with_buttons: bool,
) {
    let room = (rect.width as usize).saturating_sub(2);
    // Right end: the role or "invited"; on the selected row what can be done.
    let mut tag = role_label(member.role).to_owned();
    if member.invited {
        tag = if tag.is_empty() {
            "invited".to_owned()
        } else {
            format!("{tag} · invited")
        };
    }
    let mut buttons: Vec<(&str, Click)> = Vec::new();
    if with_buttons && !member.own {
        buttons.push((" chat ", Click::MemberChat));
        if member.removable {
            buttons.push((" remove ", Click::MemberRemove));
        }
    }
    let buttons_width: usize = buttons.iter().map(|(label, _)| width(label) + 1).sum();
    let mut buttons_width = buttons_width;
    let tag_width = width(&tag) + usize::from(!tag.is_empty());
    // What does not fit gives way: first the tag next to the buttons, then the buttons.
    if buttons_width + tag_width + 12 > room {
        if buttons_width + 12 > room {
            buttons.clear();
            buttons_width = 0;
        } else {
            tag.clear();
        }
    }
    let tag_width = width(&tag) + usize::from(!tag.is_empty());
    let left = room.saturating_sub(tag_width + buttons_width);
    // The same on every row, whatever stands at its right end.
    let name_width = (room * 2 / 5).clamp(8, 26).min(left);
    let mut name = member.name.clone();
    if member.own {
        name.push_str(" (you)");
    }
    // Cut and padded by what they take on screen: names may hold wide characters.
    let name = format!("{} ", fit(&name, name_width.saturating_sub(1)));
    let id_width = left.saturating_sub(name_width);
    let id = fit(&member.user_id, id_width.saturating_sub(1));
    let used = 2 + width(&name) + width(&id);
    let mut spans = vec![
        marker,
        Span::styled(name, name_style),
        Span::styled(id, theme.style(Role::Muted)),
    ];
    let role = if member.role > crate::matrix::MemberRole::User {
        Role::StatusKey
    } else {
        Role::Muted
    };
    let pad = (rect.width as usize).saturating_sub(used + width(&tag) + buttons_width);
    spans.push(Span::raw(" ".repeat(pad)));
    spans.push(Span::styled(tag, theme.style(role)));
    frame.render_widget(Line::from(spans), rect);
    let mut x = rect.right().saturating_sub(buttons_width as u16);
    for (label, click) in buttons {
        let w = width(label) as u16;
        let button_rect = Rect::new(x, rect.y, w, 1);
        let id = hit(click);
        let role = if app.hover == Some(id) {
            Role::Hover
        } else {
            Role::Pill
        };
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(role))),
            button_rect,
        );
        app.hits.add(button_rect, id);
        x += w + 1;
    }
}
