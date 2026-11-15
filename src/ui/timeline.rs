mod receipts;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::hit::{Button, HitId};
use crate::action::Action;
use crate::action::Pane;
use crate::app::App;
use std::collections::BTreeSet;

use crate::config::LinkPreviews;
use crate::highlight::Syntax;
use crate::images::{ImageStore, Slot};
use crate::model::{Message, MsgKind, Room, RoomKind, SendState, ThreadSummary};
use crate::previews::{Preview, PreviewStore};
use crate::rich::{self, Format, Rich, Segment};
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};

/// One screen row of the timeline.
#[derive(Debug, Clone)]
pub struct Row {
    line: Line<'static>,
    /// The message that can be selected through this row; `None` for dividers,
    /// spacing and system lines.
    msg: Option<usize>,
    /// The message this row was produced for, selectable or not.
    source: usize,
    /// Clickable column ranges inside the row.
    hits: Vec<(u16, u16, HitId)>,
    /// Set if the row is kept free for a picture.
    image: Option<ImageRow>,
}

/// One row of the space a picture takes.
#[derive(Debug, Clone, Copy)]
struct ImageRow {
    /// Which row of the picture, from its top.
    row: u16,
    /// Size of the whole picture in cells.
    cols: u16,
    rows: u16,
}

impl Row {
    fn plain(line: Line<'static>, msg: Option<usize>) -> Self {
        Self {
            line,
            msg,
            source: 0,
            hits: Vec::new(),
            image: None,
        }
    }
}

const INDENT: &str = "  ";

/// The label that copies a code block, at the right end of its first line.
const COPY_LABEL: &str = "copy";

/// "(edited)", "sending" or "not sent", shown after a message's text.
/// `repeated_shield`: the message before it, from the same sender, carries the same
/// shield, which is then not said again (in a bridged room every line would say it).
fn marker(message: &Message, repeated_shield: bool) -> Option<(String, Role)> {
    let shield = message.shield.filter(|_| !repeated_shield);
    match (message.state, message.edited, message.pinned) {
        (SendState::Sending, _, _) => Some((message.sending_label(), Role::Muted)),
        (SendState::Failed, _, _) => Some((message.failed_label(), Role::Error)),
        (SendState::Sent, edited, pinned) => {
            let role = match shield {
                Some(shield) if shield.serious() => Role::Error,
                Some(_) => Role::Muted,
                None if pinned => Role::StatusKey,
                None => Role::Muted,
            };
            let parts: Vec<&str> = shield
                .map(|shield| shield.label())
                .into_iter()
                .chain(pinned.then_some("(pinned)"))
                .chain(edited.then_some("(edited)"))
                .collect();
            (!parts.is_empty()).then(|| (parts.join(" "), role))
        }
    }
}

/// What decides whether a message gets a link preview under it.
pub struct PreviewView<'a> {
    pub store: &'a PreviewStore,
    pub mode: LinkPreviews,
    /// The room is encrypted, or not known not to be.
    pub encrypted: bool,
    pub ignored: &'a BTreeSet<String>,
}

/// The rows of a room, and the links that would be previewed if they were known.
pub struct Layout {
    pub rows: Vec<Row>,
    /// Message index and address of the link to ask the homeserver about, for each
    /// message that has one (whether or not its preview is there yet).
    pub wants: Vec<(usize, String)>,
}

/// The address that gets a preview under `message`: its first link, when previews are on
/// for this room and the message is an ordinary, sent text from someone who is not
/// ignored (a redacted message, one that could not be decrypted, a picture or a file has
/// none), and the address is one the server is asked about.
fn preview_url<'r>(message: &Message, rich: &'r Rich, view: &PreviewView) -> Option<&'r str> {
    if !view.mode.allows(view.encrypted)
        || !matches!(message.kind, MsgKind::Text | MsgKind::Emote)
        || message.state != SendState::Sent
        || message.image.is_some()
        || message.file.is_some()
        || message
            .sender_id
            .as_deref()
            .is_some_and(|id| view.ignored.contains(id))
    {
        return None;
    }
    let url = rich.links.first()?;
    crate::previews::eligible(url).then_some(url.as_str())
}

/// The rows under a message for its link's preview: the title in bold, and up to two
/// lines of description. Plain text only: it is from whoever wrote the page.
fn preview_rows(preview: &Preview, msg: usize, body_width: usize, theme: &Theme) -> Vec<Row> {
    const BAR: &str = "│ ";
    let room = body_width.saturating_sub(width(BAR)).max(1);
    let row = |text: String, style: Style| Row {
        line: Line::from(vec![
            Span::raw(INDENT),
            Span::styled(BAR, theme.style(Role::Muted)),
            Span::styled(text, style),
        ]),
        msg: Some(msg),
        source: msg,
        hits: Vec::new(),
        image: None,
    };
    let title = Style {
        fg: theme.style(Role::Text).fg,
        ..Style::new()
    }
    .add_modifier(Modifier::BOLD);
    let mut rows = vec![row(truncate(&preview.title, room), title)];
    let mut lines = wrap(&preview.description, room);
    let more = lines.len() > 2;
    lines.truncate(2);
    if more && let Some(last) = lines.last_mut() {
        *last = truncate(&format!("{last}…"), room);
    }
    for line in lines.into_iter().filter(|line| !line.is_empty()) {
        rows.push(row(line, theme.style(Role::Muted)));
    }
    rows
}

/// Lays out a room's messages. `revealed` is the message whose spoilers are shown;
/// `images` says how much space each picture gets.
#[cfg(test)]
pub fn build_rows(
    room: &Room,
    max_width: usize,
    theme: &Theme,
    revealed: Option<usize>,
    images: &ImageStore,
) -> Vec<Row> {
    build_layout(room, max_width, theme, revealed, images, None).rows
}

/// [`build_rows`], with link previews when `previews` says so.
pub fn build_layout(
    room: &Room,
    max_width: usize,
    theme: &Theme,
    revealed: Option<usize>,
    images: &ImageStore,
    previews: Option<&PreviewView>,
) -> Layout {
    let mut wants = Vec::new();
    let body_width = max_width.saturating_sub(INDENT.len()).max(1);
    // Foreground only, so the row's background (selection, hover) shows through.
    let text_style = Style {
        fg: theme.style(Role::Text).fg,
        ..Style::new()
    };
    let mut rows = Vec::new();
    // What the previous row showed, to decide on day dividers and sender headers.
    let mut last_day: Option<&str> = None;
    let mut last_sender: Option<&str> = None;

    for (i, message) in room.messages.iter().enumerate() {
        let start = rows.len();
        let repeated_shield = i > 0 && message.shield.is_some() && {
            let before = &room.messages[i - 1];
            before.shield == message.shield
                && before.sender == message.sender
                && before.day == message.day
        };
        'message: {
            match message.kind {
                MsgKind::ReadMarker => {
                    let label = " new ";
                    let fill = max_width.saturating_sub(width(label));
                    let left = fill / 2;
                    rows.push(Row::plain(
                        Line::from(Span::styled(
                            format!("{}{label}{}", "─".repeat(left), "─".repeat(fill - left)),
                            theme.style(Role::BorderFocused),
                        )),
                        None,
                    ));
                    last_sender = None;
                    break 'message;
                }
                MsgKind::System => {
                    for text in wrap(&format!("· {}", message.body), body_width) {
                        rows.push(Row::plain(
                            Line::from(vec![
                                Span::raw(INDENT),
                                Span::styled(text, theme.style(Role::Muted)),
                            ]),
                            None,
                        ));
                    }
                    last_sender = None;
                    break 'message;
                }
                _ => {}
            }

            let new_day = last_day != Some(message.day.as_str());
            let is_emote = message.kind == MsgKind::Emote;
            let show_header =
                !is_emote && (new_day || last_sender != Some(message.sender.as_str()));

            if new_day {
                let label = truncate(&format!(" {} ", message.day), max_width);
                let fill = max_width.saturating_sub(width(&label));
                let left = fill / 2;
                rows.push(Row::plain(
                    Line::from(Span::styled(
                        format!("{}{label}{}", "─".repeat(left), "─".repeat(fill - left)),
                        theme.style(Role::DateDivider),
                    )),
                    None,
                ));
            } else if show_header || (is_emote && last_sender.is_some()) {
                rows.push(Row::plain(Line::default(), None));
            }
            last_day = Some(message.day.as_str());
            // An action line stands alone: whoever speaks next gets a header again.
            last_sender = (!is_emote).then_some(message.sender.as_str());

            if show_header {
                let color = Style::new().fg(theme.user_color(&message.sender));
                let mut spans = vec![Span::styled("● ", color)];
                let with_time = 2 + width(&message.sender) + 2 + width(&message.time) <= max_width;
                let name = truncate(&message.sender, max_width.saturating_sub(2));
                let hit_w = (2 + width(&name)).min(max_width) as u16;
                spans.push(Span::styled(name, color.add_modifier(Modifier::BOLD)));
                if with_time {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(
                        message.time.clone(),
                        theme.style(Role::Timestamp),
                    ));
                }
                let mut row = Row::plain(Line::from(spans), Some(i));
                if message.sender_id.is_some() && hit_w > 0 {
                    row.hits.push((0, hit_w, HitId::Sender(i)));
                }
                rows.push(row);
            }

            if let Some((sender, quoted)) = &message.reply_to {
                let quoted = quoted.replace('\n', " ");
                let quote = if sender.is_empty() {
                    format!("┃ {quoted}")
                } else {
                    format!("┃ {sender}: {quoted}")
                };
                let shown = truncate(&quote, body_width);
                let start = INDENT.len() as u16;
                let hits = if message.reply_event.is_some() {
                    vec![(start, start + width(&shown) as u16, HitId::ReplyQuote(i))]
                } else {
                    Vec::new()
                };
                rows.push(Row {
                    line: Line::from(vec![
                        Span::raw(INDENT),
                        Span::styled(shown, theme.style(Role::ReplyQuote)),
                    ]),
                    msg: Some(i),
                    source: i,
                    hits,
                    image: None,
                });
            }

            // A picture: rows are kept free here and painted over when drawn. Its
            // caption follows as the text; without pictures the text names the file.
            let mut body_text = message.body.as_str();
            let mut text_wanted = true;
            if let Some(image) = &message.image {
                let slot = images.slot(image, body_width);
                let start = INDENT.len() as u16;
                match &slot {
                    Slot::Cells(cols, count) => {
                        for row in 0..*count {
                            rows.push(Row {
                                line: Line::from(Span::raw(INDENT)),
                                msg: Some(i),
                                source: i,
                                hits: vec![(start, start + cols, HitId::Image(i))],
                                image: Some(ImageRow {
                                    row,
                                    cols: *cols,
                                    rows: *count,
                                }),
                            });
                        }
                    }
                    Slot::Failed(reason) => {
                        let text = truncate(&format!("🖼  {} — {reason}", image.name), body_width);
                        let end = start + width(&text) as u16;
                        rows.push(Row {
                            line: Line::from(vec![
                                Span::raw(INDENT),
                                Span::styled(text, theme.style(Role::Muted)),
                            ]),
                            msg: Some(i),
                            source: i,
                            // Clicking tries again, through the viewer.
                            hits: vec![(start, end, HitId::Image(i))],
                            image: None,
                        });
                    }
                    Slot::Off => {}
                }
                if slot != Slot::Off {
                    body_text = image.caption.as_deref().unwrap_or("");
                    text_wanted = !body_text.is_empty();
                }
            }

            let (body_style, intro) = match message.kind {
                MsgKind::Emote => (
                    Style::new().fg(theme.user_color(&message.sender)),
                    Some(format!("* {} ", message.sender)),
                ),
                MsgKind::Redacted | MsgKind::Undecryptable => (theme.style(Role::ReplyQuote), None),
                _ => (text_style, None),
            };
            let mut rich = Rich::of(message.html.as_deref(), body_text);
            crate::highlight::colour(&mut rich);
            if let (Some(intro), Some(first)) = (intro, rich.lines.first_mut()) {
                first.segments.insert(
                    0,
                    Segment {
                        text: intro,
                        format: Format::default(),
                    },
                );
            }
            let reveal = revealed == Some(i);
            let mut wrapped = if text_wanted {
                rich::wrap(&rich, body_width)
            } else {
                // A picture without a caption; a marker still gets its row.
                if let Some((label, role)) = marker(message, repeated_shield) {
                    rows.push(Row::plain(
                        Line::from(vec![
                            Span::raw(INDENT),
                            Span::styled(truncate(&label, body_width), theme.style(role)),
                        ]),
                        Some(i),
                    ));
                }
                Vec::new()
            };
            let last = wrapped.len().saturating_sub(1);
            // The block the previous row belonged to, to find the first row of each.
            let mut in_block = None;
            for (n, row) in wrapped.drain(..).enumerate() {
                let first_of = row
                    .block
                    .filter(|b| in_block != Some(*b))
                    .filter(|b| rich.code_blocks.get(*b).is_some_and(|c| !c.text.is_empty()));
                in_block = row.block;
                let mut spans = vec![Span::raw(INDENT)];
                let mut hits = Vec::new();
                let mut col = INDENT.len();
                if !row.prefix.is_empty() {
                    col += width(&row.prefix);
                    spans.push(Span::styled(row.prefix, theme.style(Role::Muted)));
                }
                for segment in row.segments {
                    let segment_width = width(&segment.text);
                    let (text, style) = styled(&segment, body_style, theme, reveal);
                    if let Some(link) = segment.format.link {
                        hits.push((
                            col as u16,
                            (col + segment_width) as u16,
                            HitId::Link(i, link),
                        ));
                    }
                    col += segment_width;
                    spans.push(Span::styled(text, style));
                }
                // "(edited)", "sending" and "not sent" go after the text, or on a row of
                // their own when there is no room left.
                let mut own_row = None;
                if n == last
                    && let Some((label, role)) = marker(message, repeated_shield)
                {
                    if col + 1 + width(&label) <= max_width {
                        spans.push(Span::raw(" "));
                        col += 1 + width(&label);
                        spans.push(Span::styled(label, theme.style(role)));
                    } else {
                        own_row = Some(Row::plain(
                            Line::from(vec![
                                Span::raw(INDENT),
                                Span::styled(truncate(&label, body_width), theme.style(role)),
                            ]),
                            Some(i),
                        ));
                    }
                }
                // "copy" at the right end of a code block's first line, where it fits.
                if let Some(block) = first_of {
                    let label_width = width(COPY_LABEL);
                    if col + 1 + label_width <= max_width {
                        let start = max_width - label_width;
                        spans.push(Span::raw(" ".repeat(start - col)));
                        spans.push(Span::styled(COPY_LABEL, theme.style(Role::Muted)));
                        hits.push((
                            start as u16,
                            (start + label_width) as u16,
                            HitId::CopyCode(i, block),
                        ));
                    }
                }
                rows.push(Row {
                    line: Line::from(spans),
                    msg: Some(i),
                    source: i,
                    hits,
                    image: None,
                });
                rows.extend(own_row);
            }

            // The first link's preview, under the text.
            if let Some(view) = previews
                && let Some(url) = preview_url(message, &rich, view)
            {
                wants.push((i, url.to_owned()));
                if let Some(preview) = view.store.get(url) {
                    rows.extend(preview_rows(preview, i, body_width, theme));
                }
            }

            // Reaction chips, wrapped onto further rows when they do not fit.
            let mut spans = vec![Span::raw(INDENT)];
            let mut hits = Vec::new();
            let mut col = INDENT.len();
            for (r, reaction) in message.reactions.iter().enumerate() {
                let chip = truncate(
                    &format!(" {} {} ", reaction.key, reaction.count),
                    body_width,
                );
                let chip_width = width(&chip);
                if col + chip_width > max_width && !hits.is_empty() {
                    rows.push(Row {
                        line: Line::from(std::mem::replace(&mut spans, vec![Span::raw(INDENT)])),
                        msg: Some(i),
                        source: i,
                        hits: std::mem::take(&mut hits),
                        image: None,
                    });
                    col = INDENT.len();
                }
                let role = if reaction.own {
                    Role::ReactionOwn
                } else {
                    Role::Reaction
                };
                spans.push(Span::styled(chip, theme.style(role)));
                hits.push((col as u16, (col + chip_width) as u16, HitId::Reaction(i, r)));
                col += chip_width;
                if col < max_width {
                    spans.push(Span::raw(" "));
                    col += 1;
                }
            }
            if !hits.is_empty() {
                rows.push(Row {
                    line: Line::from(spans),
                    msg: Some(i),
                    source: i,
                    hits,
                    image: None,
                });
            }
            if let Some(thread) = &message.thread {
                rows.push(thread_row(thread, i, body_width, theme));
            }
        }
        for row in &mut rows[start..] {
            row.source = i;
        }
    }
    Layout { rows, wants }
}

/// The line under a message that started a thread: how many replies, and who said
/// what last. Clicking it opens the thread.
fn thread_row(thread: &ThreadSummary, msg: usize, body_width: usize, theme: &Theme) -> Row {
    let count = match thread.replies {
        1 => "💬 1 reply".to_owned(),
        n => format!("💬 {n} replies"),
    };
    let count = truncate(&count, body_width);
    let mut link = Style {
        fg: theme.style(Role::Link).fg,
        ..Style::new()
    };
    let mut used = width(&count);
    let mut spans = vec![Span::raw(INDENT)];
    if thread.unread {
        link = link.add_modifier(Modifier::BOLD);
    }
    spans.push(Span::styled(count, link));
    let mut more = |text: String, style: Style| {
        let text = truncate(&text, body_width - used);
        // Not worth a lone "…".
        if width(&text) > 1 {
            used += width(&text);
            spans.push(Span::styled(text, style));
        }
    };
    if thread.unread {
        more(" · new".to_owned(), link);
    }
    if let Some((sender, text)) = &thread.latest {
        let text = text.replace('\n', " ");
        more(
            format!(" · {sender}: {text}"),
            theme.style(Role::ReplyQuote),
        );
    }
    let start = INDENT.len() as u16;
    Row {
        line: Line::from(spans),
        msg: Some(msg),
        source: msg,
        hits: vec![(start, start + used as u16, HitId::Thread(msg))],
        image: None,
    }
}

/// Text and style for one formatted piece.
fn styled(segment: &Segment, base: Style, theme: &Theme, reveal: bool) -> (String, Style) {
    let format = &segment.format;
    if format.spoiler && !reveal {
        // Covered until the message is selected.
        return ("▒".repeat(width(&segment.text)), theme.style(Role::Spoiler));
    }
    let mut style = base;
    if format.code {
        style = style.patch(theme.style(Role::Code));
    }
    if let Some(syntax) = format.syntax {
        style = style.patch(theme.style(match syntax {
            Syntax::Plain => Role::CodeText,
            Syntax::Keyword => Role::CodeKeyword,
            Syntax::String => Role::CodeString,
            Syntax::Comment => Role::CodeComment,
            Syntax::Constant => Role::CodeConstant,
            Syntax::Type => Role::CodeType,
            Syntax::Function => Role::CodeFunction,
        }));
    }
    if format.link.is_some() {
        style = style.patch(theme.style(Role::Link));
    }
    if let Some((r, g, b)) = format.color {
        style = style.fg(theme.color(r, g, b));
    }
    if format.spoiler {
        style = style
            .patch(theme.style(Role::Spoiler))
            .fg(base.fg.unwrap_or_default());
    }
    for (on, modifier) in [
        (format.bold, Modifier::BOLD),
        (format.italic, Modifier::ITALIC),
        (format.strike, Modifier::CROSSED_OUT),
        (format.underline, Modifier::UNDERLINED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    (segment.text.clone(), style)
}

/// Everything the rows depend on: they are laid out again when any of it changes.
#[derive(Debug, PartialEq)]
struct RowKey {
    room: String,
    messages: u64,
    width: usize,
    /// Changes whenever a theme is (re)loaded, even under the same name.
    theme: String,
    revealed: Option<usize>,
    /// Changes when a picture arrives or fails, which may change its height.
    images: u64,
    /// What decides about link previews: one arrives, the mode or the room's encryption
    /// changes, or a sender is ignored.
    previews: (u64, LinkPreviews, bool, usize),
}

/// The laid-out rows of the open room, reused between frames while nothing that
/// affects them has changed.
#[derive(Debug)]
pub struct RowCache {
    key: RowKey,
    rows: Vec<Row>,
    /// The links to ask the homeserver about (see `Layout::wants`).
    wants: Vec<(usize, String)>,
    /// What was at the bottom of the view when it was last drawn scrolled up.
    anchor: Option<Anchor>,
}

/// Pins the view to a message while the rows around it change: new messages below,
/// a picture whose size becomes known, another width.
#[derive(Debug, Clone)]
struct Anchor {
    /// The message the bottom row of the view belonged to.
    id: String,
    /// Rows of that message below the bottom row of the view.
    below: usize,
    /// The scroll position this was recorded at.
    scroll: usize,
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(room) = app.rooms.get(app.active) else {
        let block = super::pane_block(app, Pane::Timeline, "No room");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        app.hits.add(area, HitId::Pane(Pane::Timeline));
        let line = Line::from(Span::styled(" No rooms yet.", app.theme.style(Role::Muted)));
        frame.render_widget(line, inner);
        return;
    };
    let prefix = if room.direct { "@" } else { "#" };
    let in_thread = app.threads.open.is_some();
    let title = if in_thread {
        format!("Thread · {prefix}{}", room.name)
    } else {
        format!("{prefix}{}", room.name)
    };
    let mut block = super::pane_block(app, Pane::Timeline, &title);
    if let Some(who) = &room.typing {
        block = block.title_bottom(Line::from(Span::styled(
            format!(" {who} is typing… "),
            app.theme.style(Role::Muted),
        )));
    }
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    app.hits.add(area, HitId::Pane(Pane::Timeline));
    super::roominfo::title_hit(&mut app.hits, area, &title);
    // The mouse's way out of a thread, at the right end of the title row.
    let back = " ← back ";
    let back_width = width(back) as u16;
    if in_thread && area.width > back_width + width(&title) as u16 + 8 {
        let rect = Rect::new(area.right() - back_width - 2, area.y, back_width, 1);
        let role = if app.hover == Some(HitId::Do(Action::CloseThread)) {
            Role::Hover
        } else {
            Role::Pill
        };
        frame.render_widget(Line::from(Span::styled(back, app.theme.style(role))), rect);
        app.hits.add(rect, HitId::Do(Action::CloseThread));
    }
    if inner.width < 2 || inner.height == 0 {
        return;
    }
    let theme = &app.theme;
    if room.is_pending() {
        let (prompt, actions): (&str, &[(&str, Action)]) = if room.kind == RoomKind::Invite {
            (
                " You are invited to this room.",
                &[
                    (" Accept ", Action::AcceptInvite),
                    (" Decline ", Action::DeclineInvite),
                ],
            )
        } else {
            (
                " You asked to join this room — waiting for approval.",
                &[(" Withdraw ", Action::WithdrawKnock)],
            )
        };
        let line = Line::from(Span::styled(prompt, theme.style(Role::Muted)));
        frame.render_widget(line, inner);
        if inner.height > 2 {
            let mut x = inner.x + 1;
            for &(label, action) in actions {
                let w = (width(label) as u16).min(inner.right().saturating_sub(x));
                let rect = Rect::new(x, inner.y + 2, w, 1);
                frame.render_widget(
                    Line::from(Span::styled(label, theme.style(Role::Pill))),
                    rect,
                );
                app.hits.add(rect, HitId::Do(action));
                x += w + 2;
            }
        }
        return;
    }
    if app.timeline.detached {
        let banner_rect = Rect::new(inner.x, inner.y, inner.width, 1);
        let new_note = if app.timeline.detached_new > 0 {
            format!(" · {} new", app.timeline.detached_new)
        } else {
            String::new()
        };
        let text = match app.keymap.jump_to_newest_label() {
            Some(key) => {
                format!(" viewing older messages{new_note} · {key} / click to return to now")
            }
            None => format!(" viewing older messages{new_note} · click to return to now"),
        };
        let label = truncate(&text, banner_rect.width as usize);
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(Role::Banner))),
            banner_rect,
        );
        app.hits
            .add(banner_rect, HitId::Button(Button::JumpToNewest));
        inner.y += 1;
        inner.height = inner.height.saturating_sub(1);
        if inner.height == 0 {
            return;
        }
    }
    if room.messages.is_empty() {
        let ready =
            !app.timeline.loading_older && (app.sync.is_none() || app.timeline.reached_start);
        let text = if ready {
            " No messages yet."
        } else {
            " Loading messages…"
        };
        frame.render_widget(
            Line::from(Span::styled(text, theme.style(Role::Muted))),
            inner,
        );
        // Ask for history: an empty view may just not have loaded any yet.
        app.timeline.want_older = true;
        app.timeline.want_newer = false;
        app.timeline.view_full = false;
        if ready {
            // Nothing visible exists (e.g. an empty room or all messages ignored),
            // so treat the frame as drawn to let unread/read-marker settling finish.
            app.timeline.drawn = true;
        }
        return;
    }

    // The last column is kept free for the scrollbar.
    let content = Rect::new(inner.x, inner.y, inner.width - 1, inner.height);
    // Spoilers are uncovered while their message is selected.
    let revealed = app
        .timeline
        .selected
        .filter(|i| room.messages.get(*i).is_some_and(|m| m.has_spoiler()));
    let key = RowKey {
        room: room.id.clone(),
        messages: app.timeline_rev,
        width: content.width as usize,
        theme: app.theme_rev.to_string(),
        revealed,
        images: app.images.layout_rev,
        previews: (
            app.previews.rev,
            app.link_previews,
            room.encrypted,
            app.ignored_users.len(),
        ),
    };
    let previous = app.row_cache.take();
    let mut cache = match previous {
        Some(cache) if cache.key == key => cache,
        previous => {
            let view = PreviewView {
                store: &app.previews,
                mode: app.link_previews,
                encrypted: room.encrypted,
                ignored: &app.ignored_users,
            };
            let Layout { mut rows, wants } = build_layout(
                room,
                content.width as usize,
                theme,
                revealed,
                &app.images,
                Some(&view),
            );
            if app.read_markers {
                rows = receipts::add(rows, room, content.width as usize, theme);
            }
            // Reading older messages (or paging forwards in a detached view): keep the
            // same bottom row on screen when messages arrive below or change height.
            if let Some(anchor) = previous
                .filter(|old| old.key.room == key.room)
                .and_then(|old| old.anchor)
                && (app.timeline.scroll > 0 || app.timeline.detached)
                && let Some(at) = room.messages.iter().position(|m| m.id == anchor.id)
                && let Some(last) = rows.iter().rposition(|row| row.source == at)
                && let Some(first) = rows.iter().position(|row| row.source == at)
            {
                let bottom = last.saturating_sub(anchor.below).max(first);
                let pinned = rows.len() - 1 - bottom;
                // What the user scrolled since then still counts.
                app.timeline.scroll = pinned
                    .saturating_add(app.timeline.scroll)
                    .saturating_sub(anchor.scroll);
            }
            RowCache {
                key,
                rows,
                wants,
                anchor: None,
            }
        }
    };
    let rows = &cache.rows;
    let height = content.height as usize;
    let max_scroll = rows.len().saturating_sub(height);

    let view = &mut app.timeline;
    view.drawn = true;
    view.view_full = rows.len() > height;
    view.page = height;
    view.scroll = view.scroll.min(max_scroll);
    let mut top = max_scroll - view.scroll;
    if view.reveal_unread {
        view.reveal_unread = false;
        // The "new" line goes to the top of the view when what is below it does not
        // fit; when it does, the newest message stays at the bottom.
        if let Some(at) = room
            .messages
            .iter()
            .position(|m| m.kind == MsgKind::ReadMarker)
            && let Some(line) = rows.iter().position(|row| row.source == at)
        {
            top = line.min(max_scroll);
            view.scroll = max_scroll - top;
        }
    }
    if view.reveal_selected {
        view.reveal_selected = false;
        if let Some(selected) = view.selected {
            let first = rows.iter().position(|r| r.msg == Some(selected));
            let last = rows.iter().rposition(|r| r.msg == Some(selected));
            if let (Some(first), Some(last)) = (first, last) {
                // The find bar lies over the top rows; a found message goes below it.
                let covered = if matches!(app.modal, Some(crate::modal::Modal::Search(_))) {
                    4.min(height.saturating_sub(1))
                } else {
                    0
                };
                if first < top + covered {
                    top = first.saturating_sub(covered);
                } else if last >= top + height {
                    top = (last + 1 - height).min(first);
                }
                view.scroll = max_scroll - top.min(max_scroll);
                top = max_scroll - view.scroll;
            }
        }
    }
    let selected = view.selected;
    let scroll = view.scroll;

    // Showing the oldest loaded row: more history is wanted.
    view.want_older = top == 0;
    // Showing the newest loaded row in a detached slice: more forward history is wanted.
    view.want_newer = view.detached && !view.reached_end && scroll == 0;

    // A short conversation sits at the bottom, next to the composer.
    let pad = height.saturating_sub(rows.len()) as u16;
    for (n, row) in rows.iter().skip(top).take(height).enumerate() {
        let rect = Rect::new(content.x, content.y + pad + n as u16, content.width, 1);
        if let Some(msg) = row.msg {
            if selected == Some(msg) {
                frame
                    .buffer_mut()
                    .set_style(rect, theme.style(Role::MessageSelected));
            } else if matches!(app.hover, Some(HitId::Message(m) | HitId::Reaction(m, _) | HitId::Link(m, _) | HitId::Image(m) | HitId::Thread(m) | HitId::CopyCode(m, _) | HitId::ReplyQuote(m) | HitId::Sender(m)) if m == msg)
            {
                frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
            }
            app.hits.add(rect, HitId::Message(msg));
        }
        frame.render_widget(&row.line, rect);
        for (start, end, id) in &row.hits {
            let start = (*start).min(rect.width);
            let end = (*end).min(rect.width);
            app.hits
                .add(Rect::new(rect.x + start, rect.y, end - start, 1), *id);
        }
    }

    // Pictures are painted over the rows kept free for them; what is scrolled out of
    // the view is left out.
    let covered = app.modal.is_some() || app.help_open || app.images.viewer_open();
    let indent = INDENT.len() as u16;
    let last_visible = (top + height).min(rows.len());
    let mut n = top;
    while n < last_visible {
        let Some(slot) = rows[n].image else {
            n += 1;
            continue;
        };
        let source = rows[n].source;
        let count = rows[n..last_visible]
            .iter()
            .take_while(|row| row.source == source && row.image.is_some())
            .count();
        if let Some(info) = room.messages.get(source).and_then(|m| m.image.as_ref()) {
            let rect = Rect::new(
                content.x + indent,
                content.y + pad + (n - top) as u16,
                content.width.saturating_sub(indent),
                count as u16,
            );
            let inline = super::image::Inline {
                info,
                area: rect,
                skip: slot.row,
                size: (slot.cols, slot.rows),
                covered,
            };
            super::image::draw_inline(frame, &mut app.images, theme, inline);
        }
        n += count;
    }
    // Links of the messages that are on screen are asked about (a few at a time, once
    // each: see `PreviewStore`); the answers lay the rows out again.
    if app.can_fetch_previews()
        && !cache.wants.is_empty()
        && let (Some(first), Some(last)) = (rows.get(top), rows.get(last_visible.wrapping_sub(1)))
    {
        let on_screen = first.source..=last.source;
        for (msg, url) in &cache.wants {
            if on_screen.contains(msg) {
                app.previews.request(url);
            }
        }
    }
    // Pictures within a page of the view are fetched ahead.
    let near = top.saturating_sub(height)..(top + 2 * height).min(rows.len());
    for row in &rows[near] {
        if row.image.is_some_and(|slot| slot.row == 0)
            && let Some(info) = room.messages.get(row.source).and_then(|m| m.image.as_ref())
        {
            app.images.request(info);
        }
    }

    // Remember what is at the bottom of a scrolled or detached view, to stay there (see `Anchor`).
    let anchor = ((scroll > 0 || app.timeline.detached) && last_visible > 0)
        .then(|| {
            let bottom = last_visible - 1;
            let source = rows[bottom].source;
            let id = room.messages.get(source)?.id.clone();
            let below = rows[bottom + 1..]
                .iter()
                .take_while(|row| row.source == source)
                .count();
            (!id.is_empty()).then_some(Anchor { id, below, scroll })
        })
        .flatten();

    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll + 1).position(top);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(theme.style(Role::Scrollbar))
            .thumb_style(theme.style(Role::ScrollbarThumb));
        frame.render_stateful_widget(bar, inner, &mut state);
    }

    if app.timeline.loading_older && content.width > 24 {
        let label = " loading older messages… ";
        let rect = Rect::new(content.x + 1, content.y, width(label) as u16, 1);
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(Role::Pill))),
            rect,
        );
    }

    if app.timeline.loading_newer && content.width > 26 && (scroll == 0 || content.width > 38) {
        let label = " loading newer messages… ";
        let rect = Rect::new(content.x + 1, content.bottom() - 1, width(label) as u16, 1);
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(Role::Pill))),
            rect,
        );
    }

    if scroll > 0 {
        let label = " ↓ newest ";
        let label_width = width(label) as u16;
        if content.width > label_width + 1 {
            let rect = Rect::new(
                content.right() - label_width - 1,
                content.bottom() - 1,
                label_width,
                1,
            );
            let line = Line::from(Span::styled(label, theme.style(Role::Pill)));
            frame.render_widget(&line, rect);
            app.hits.add(rect, HitId::Button(Button::JumpToNewest));
        }
    }
    cache.anchor = anchor;
    app.row_cache = Some(cache);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::images::Graphics;
    use crate::model::demo_rooms;
    use crate::theme::DEFAULT_THEME;

    #[test]
    fn shields_are_marked_after_the_text() {
        use crate::model::Shield;
        let mut message = Message {
            body: "hello".into(),
            ..Message::default()
        };
        assert_eq!(marker(&message, false), None);
        message.shield = Some(Shield::UnverifiedDevice);
        message.edited = true;
        assert_eq!(
            marker(&message, false),
            Some((
                "⚠ from an unverified device (edited)".to_owned(),
                Role::Error
            ))
        );
        // Said once for a run of messages from the same sender, not on every line.
        assert_eq!(
            marker(&message, true),
            Some(("(edited)".to_owned(), Role::Muted))
        );
        // Not a warning, only a remark; and it shows up in the laid-out rows.
        message.shield = Some(Shield::NotEncrypted);
        message.edited = false;
        assert_eq!(
            marker(&message, false),
            Some(("not encrypted".to_owned(), Role::Muted))
        );
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let mut room = demo_rooms().remove(0);
        room.messages = vec![message];
        let images = ImageStore::new(Graphics::halfblocks());
        let text: String = build_rows(&room, 60, &theme, None, &images)
            .iter()
            .map(|row| row.line.to_string())
            .collect();
        assert!(
            text.contains("hello") && text.contains("not encrypted"),
            "{text}"
        );
    }

    #[test]
    fn rows_fit_and_group_by_sender() {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let room = &demo_rooms()[0];
        let images = ImageStore::new(Graphics::halfblocks());
        for max in [10, 20, 37, 80] {
            let rows = build_rows(room, max, &theme, None, &images);
            for row in &rows {
                assert!(row.line.width() <= max, "width {max}: {:?}", row.line);
            }
            // Two consecutive messages by bob share one header.
            let headers = rows
                .iter()
                .filter(|r| r.line.spans.first().is_some_and(|s| s.content == "● "))
                .count();
            assert_eq!(headers, room.messages.len() - 1);
        }
    }

    #[test]
    fn thread_line_fits_any_width_and_is_one_click_target() {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        let mut room = demo_rooms().remove(0);
        room.messages[1].thread = Some(ThreadSummary {
            replies: 12,
            latest: Some(("carol".into(), "a long answer\nover two lines".into())),
            unread: true,
            demo_replies: Vec::new(),
        });
        for max in [3, 4, 9, 14, 20, 31, 80] {
            let rows = build_rows(&room, max, &theme, None, &images);
            let lines: Vec<&Row> = rows
                .iter()
                .filter(|row| row.hits.iter().any(|hit| hit.2 == HitId::Thread(1)))
                .collect();
            assert_eq!(lines.len(), 1, "width {max}");
            let row = lines[0];
            assert!(row.line.width() <= max, "width {max}: {:?}", row.line);
            assert_eq!(row.msg, Some(1));
            let (start, end, _) = row.hits[0];
            assert_eq!((start as usize, end as usize), (2, row.line.width()));
        }
        let rows = build_rows(&room, 80, &theme, None, &images);
        let text: Vec<String> = rows.iter().map(|row| row.line.to_string()).collect();
        assert!(
            text.contains(
                &"  💬 12 replies · new · carol: a long answer over two lines".to_owned()
            ),
            "{text:?}"
        );
    }
}
