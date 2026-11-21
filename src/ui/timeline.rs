mod memo;
mod notes;
mod receipts;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::hit::{Button, HitId};
use crate::action::Action;
use crate::action::Pane;
use crate::app::App;
use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::config::LinkPreviews;
use crate::highlight::Syntax;
use crate::images::{EMOTE_CELLS, ImageStore, Slot};
use crate::matrix::{EmoteCatalog, is_valid_mxc};
#[cfg(test)]
use crate::model::Room;
use crate::model::{Message, MsgKind, RoomKind, SendState, ThreadSummary};
use crate::previews::{Preview, PreviewStore};
use crate::rich::{self, Format, Rich, Segment};
use crate::text::{truncate, width, wrap};
use crate::theme::{Role, Theme};

/// One screen row of the timeline.
#[derive(Clone)]
#[cfg_attr(test, derive(PartialEq))]
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
    /// Cells of the row that custom emoji pictures are painted over.
    emotes: Vec<EmoteSpot>,
    /// What of a voice or audio message the row shows, which looks different while
    /// the message loads or plays (see `ui::audio`).
    audio: Option<AudioPart>,
}

/// A row of a voice or audio message that changes while it loads or plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AudioPart {
    /// Of its first line (`▶ 🎤 note.ogg · 0:12 · 2 kB`), else of its waveform's.
    head: bool,
    /// The first row of that line (a long one takes more).
    first: bool,
    /// Which row of that line, and how many rows it took.
    nth: u16,
    of: u16,
    /// Columns at the row's end that a read marker takes (see `receipts`): it stays.
    marker: u16,
}

/// Hand-written: the line is what the message says, and who sent it.
impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Row")
            .field("width", &self.line.width())
            .field("msg", &self.msg)
            .field("source", &self.source)
            .field("hits", &self.hits)
            .field("image", &self.image)
            .field("emotes", &self.emotes.len())
            .field("audio", &self.audio)
            .finish_non_exhaustive()
    }
}

/// The two cells of a row that a custom emoji's picture goes over.
#[derive(Clone)]
#[cfg_attr(test, derive(PartialEq))]
struct EmoteSpot {
    /// Column of the first cell, from the row's left edge.
    col: u16,
    /// The picture's `mxc://` address.
    url: String,
}

/// Hand-written: the address stays out of logs.
impl std::fmt::Debug for EmoteSpot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmoteSpot")
            .field("col", &self.col)
            .finish_non_exhaustive()
    }
}

/// One row of the space a picture takes.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(test, derive(PartialEq))]
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
            emotes: Vec::new(),
            audio: None,
        }
    }

    /// Makes the row one of the message at `to`: the row's message is at another place
    /// among the room's now (see `memo`).
    fn renumber(&mut self, to: usize) {
        if self.msg.is_some() {
            self.msg = Some(to);
        }
        self.source = to;
        for hit in &mut self.hits {
            hit.2 = message_target(hit.2, to).1;
        }
    }
}

const INDENT: &str = "  ";

/// Whether a reaction is made with a custom emoji: its key is an `mxc://` address.
fn is_mxc_key(key: &str) -> bool {
    key.trim_start()
        .get(..6)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mxc://"))
}

/// What a reaction chip shows without a picture. A custom emoji shows by its shortcode
/// where the packs know it, and never by its address.
fn reaction_label(key: &str, emotes: &EmoteCatalog) -> String {
    if !is_mxc_key(key) {
        return key.to_owned();
    }
    match emotes.shortcode_for_url(key.trim()) {
        Some(code) => format!(":{code}:"),
        None => rich::EMOTICON_PLACEHOLDER.to_owned(),
    }
}

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
                .chain(message.saved.then_some("(saved)"))
                .chain(edited.then_some("(edited)"))
                .collect();
            (!parts.is_empty()).then(|| (parts.join(" "), role))
        }
    }
}

/// What a row of the sound's message shows while it loads or plays, in `width`
/// columns (`full`: those of a row without a read marker); `None` where the row stays
/// as it is. A name too long for the first row goes on over the rows the first line
/// took; a waveform that took more than one row says all in its first, the others are
/// left empty.
fn sound_row(
    part: AudioPart,
    view: crate::app::AudioView,
    message: &Message,
    (width, full): (usize, usize),
    theme: &Theme,
    images: &mut ImageStore,
) -> Option<Line<'static>> {
    let file = message.file.as_ref()?;
    let waveform = file
        .audio
        .as_ref()
        .map_or(&[][..], |meta| meta.waveform.as_slice());
    match (part.head, part.first) {
        (true, _) => {
            let spinner = if view == crate::app::AudioView::Loading {
                images.spinner()
            } else {
                ""
            };
            let at = super::audio::HeadRow {
                nth: usize::from(part.nth),
                of: usize::from(part.of),
                full,
            };
            Some(super::audio::head_row(
                view, file, INDENT, width, spinner, theme, at,
            ))
        }
        (false, true) => super::audio::wave(view, waveform, INDENT, width, theme),
        (false, false) => {
            super::audio::wave(view, waveform, INDENT, width, theme).map(|_| Line::default())
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

/// What decides how custom emoji are shown.
pub struct EmoteView<'a> {
    /// The image packs: a reaction with an emoji is named after it where they know it.
    pub catalog: &'a EmoteCatalog,
    /// Nothing is fetched for what these people sent.
    pub ignored: &'a BTreeSet<String>,
}

/// The rows of a room, and the links that would be previewed if they were known.
#[derive(Default)]
pub struct Layout {
    pub rows: Vec<Row>,
    /// Message index and address of the link to ask the homeserver about, for each
    /// message that has one (whether or not its preview is there yet).
    pub wants: Vec<(usize, String)>,
    /// Message index and address of each custom emoji that is shown as a picture once
    /// it is there, in the text or on a reaction.
    pub emote_wants: Vec<(usize, String)>,
}

/// Whether the custom emoji in the text of `message` may be shown as pictures: the
/// terminal can draw them, and the message is an ordinary one from someone who is not
/// ignored (a deleted one, or one that could not be decrypted, shows nothing of them).
fn emote_pictures(message: &Message, images: &ImageStore, ignored: &BTreeSet<String>) -> bool {
    images.emote_pictures()
        && matches!(message.kind, MsgKind::Text | MsgKind::Emote)
        && !message
            .sender_id
            .as_deref()
            .is_some_and(|id| ignored.contains(id))
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

/// For a voice or audio message that can be played here: whether the second line of its
/// text is its waveform (as `matrix::convert` writes it). `None` for any other message.
fn sound_rows(message: &Message, rich: &Rich) -> Option<bool> {
    if !message.is_sound() {
        return None;
    }
    let waveform = message
        .file
        .as_ref()?
        .audio
        .as_ref()
        .map_or(&[][..], |meta| meta.waveform.as_slice());
    let second = rich.lines.get(1).map(|line| {
        line.segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>()
    });
    Some(!waveform.is_empty() && second.is_some_and(|text| text == crate::audio::bars(waveform)))
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
        emotes: Vec::new(),
        audio: None,
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
    build_layout(
        room,
        max_width,
        theme,
        revealed,
        images,
        None,
        &EmoteView {
            catalog: &EmoteCatalog::default(),
            ignored: &BTreeSet::new(),
        },
    )
    .rows
}

/// [`build_rows`], with link previews when `previews` says so. (The timeline lays its
/// rows out through `memo::lay_out`, which adds notes and read markers to them.)
#[cfg(test)]
pub fn build_layout(
    room: &Room,
    max_width: usize,
    theme: &Theme,
    revealed: Option<usize>,
    images: &ImageStore,
    previews: Option<&PreviewView>,
    emotes: &EmoteView,
) -> Layout {
    let lay = Lay::new(max_width, theme, revealed, images, previews, emotes);
    let mut layout = Layout::default();
    let mut flow = Flow::default();
    for (i, message) in room.messages.iter().enumerate() {
        let before = flow.next(&room.messages, i);
        message_rows(i, message, before, &lay, &mut layout);
    }
    layout
}

/// What all messages of a room are laid out with alike.
#[derive(Clone, Copy)]
struct Lay<'a> {
    max_width: usize,
    /// What is left of it after the indent.
    body_width: usize,
    theme: &'a Theme,
    /// Foreground only, so the row's background (selection, hover) shows through.
    text_style: Style,
    /// The message whose spoilers are shown.
    revealed: Option<usize>,
    /// How much space each picture gets, and which custom emoji pictures are there.
    images: &'a ImageStore,
    previews: Option<&'a PreviewView<'a>>,
    emotes: &'a EmoteView<'a>,
}

impl<'a> Lay<'a> {
    fn new(
        max_width: usize,
        theme: &'a Theme,
        revealed: Option<usize>,
        images: &'a ImageStore,
        previews: Option<&'a PreviewView<'a>>,
        emotes: &'a EmoteView<'a>,
    ) -> Self {
        Self {
            max_width,
            body_width: max_width.saturating_sub(INDENT.len()).max(1),
            theme,
            text_style: Style {
                fg: theme.style(Role::Text).fg,
                ..Style::new()
            },
            revealed,
            images,
            previews,
            emotes,
        }
    }
}

/// What the rows of a message take from the messages before it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
struct Before {
    /// A day divider goes first: the message before was on another day.
    day: bool,
    /// The sender's name and the time go first.
    header: bool,
    /// An empty row goes first.
    gap: bool,
    /// The message before it, from the same sender, carries the same shield.
    repeated_shield: bool,
}

/// What the rows so far showed, to decide on day dividers and sender headers.
#[derive(Default)]
struct Flow<'a> {
    last_day: Option<&'a str>,
    /// The last message of the sender whose header is the last one, while what follows
    /// is theirs. Senders are told apart by their user IDs: two people may go by the
    /// same name (see `Message::same_sender`).
    last_sender: Option<&'a Message>,
}

impl<'a> Flow<'a> {
    /// What the message at `i` takes from the messages before it; the flow moves past it.
    fn next(&mut self, messages: &'a [Message], i: usize) -> Before {
        let message = &messages[i];
        match message.kind {
            // A line of its own: whoever speaks next gets a header again.
            MsgKind::ReadMarker | MsgKind::System => {
                self.last_sender = None;
                return Before::default();
            }
            MsgKind::Text | MsgKind::Emote | MsgKind::Redacted | MsgKind::Undecryptable => {}
        }
        let repeated_shield = i > 0 && message.shield.is_some() && {
            let before = &messages[i - 1];
            before.shield == message.shield
                && before.same_sender(message)
                && before.day == message.day
        };
        let day = self.last_day != Some(message.day.as_str());
        let is_emote = message.kind == MsgKind::Emote;
        let same = self
            .last_sender
            .is_some_and(|last| last.same_sender(message));
        let header = !is_emote && (day || !same);
        let gap = !day && (header || (is_emote && self.last_sender.is_some()));
        self.last_day = Some(message.day.as_str());
        // An action line stands alone: whoever speaks next gets a header again.
        self.last_sender = (!is_emote).then_some(message);
        Before {
            day,
            header,
            gap,
            repeated_shield,
        }
    }
}

/// Lays out the message at `i` of its room: adds its rows to `out`, and the links and
/// custom emoji it wants fetched. `before`: what it takes from the messages before it.
/// Whether the colouring of its code was cut short for lack of time: laid out again, it
/// may get more colours (see [`crate::highlight::colour`]).
fn message_rows(i: usize, message: &Message, before: Before, lay: &Lay, out: &mut Layout) -> bool {
    let Lay {
        max_width,
        body_width,
        theme,
        text_style,
        revealed,
        images,
        previews,
        emotes,
    } = *lay;
    let Layout {
        rows,
        wants,
        emote_wants,
    } = out;
    let start = rows.len();
    let repeated_shield = before.repeated_shield;
    let mut cut_short = false;
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
                break 'message;
            }
            MsgKind::Text | MsgKind::Emote | MsgKind::Redacted | MsgKind::Undecryptable => {}
        }

        if before.day {
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
        } else if before.gap {
            rows.push(Row::plain(Line::default(), None));
        }

        if before.header {
            // The colour of the user ID, as the name may change or be someone else's too.
            let color = Style::new().fg(theme.user_color(message.sender_key()));
            let mut spans = vec![Span::styled("● ", color)];
            let sender = message.sender_name();
            let with_time = 2 + width(&sender) + 2 + width(&message.time) <= max_width;
            let name = truncate(&sender, max_width.saturating_sub(2));
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
                emotes: Vec::new(),
                audio: None,
            });
        }

        if let Some(poll) = &message.poll
            && message.kind != MsgKind::Redacted
        {
            push_poll_rows(
                rows,
                i,
                message,
                poll,
                body_width,
                max_width,
                theme,
                text_style,
                repeated_shield,
            );
        } else {
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
                                emotes: Vec::new(),
                                audio: None,
                            });
                        }
                    }
                    Slot::Failed(reason) if message.file.is_none() => {
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
                            emotes: Vec::new(),
                            audio: None,
                        });
                    }
                    Slot::Failed(_) | Slot::Off => {}
                }
                if slot != Slot::Off && message.file.is_none() {
                    body_text = image.caption.as_deref().unwrap_or("");
                    text_wanted = !body_text.is_empty();
                }
            }

            let playable = message.is_playable()
                && message.kind != MsgKind::Redacted
                && message.state != SendState::Failed;
            let (body_style, intro) = match message.kind {
                MsgKind::Emote => (
                    Style::new().fg(theme.user_color(message.sender_key())),
                    Some(format!("* {} ", message.sender_name())),
                ),
                MsgKind::Redacted | MsgKind::Undecryptable => (theme.style(Role::ReplyQuote), None),
                MsgKind::Text | MsgKind::System | MsgKind::ReadMarker => (text_style, None),
            };
            let mut rich = Rich::of(message.html.as_deref(), body_text);
            cut_short = crate::highlight::colour(&mut rich);
            if let (Some(intro), Some(first)) = (intro, rich.lines.first_mut()) {
                first.segments.insert(
                    0,
                    Segment {
                        text: intro,
                        format: Format::default(),
                    },
                );
            }
            if playable && let Some(first) = rich.lines.first_mut() {
                first.segments.insert(
                    0,
                    Segment {
                        text: "▶ ".to_owned(),
                        format: Format::default(),
                    },
                );
            }
            let reveal = revealed == Some(i);
            // Custom emoji get two cells for their picture where it is there to draw;
            // one under a spoiler only while the message is uncovered.
            if emote_pictures(message, images, emotes.ignored) {
                let mut hidden = vec![false; rich.emotes.len()];
                for segment in rich.lines.iter().flat_map(|line| &line.segments) {
                    if let Some(e) = segment.format.emote
                        && segment.format.spoiler
                        && !reveal
                        && let Some(hidden) = hidden.get_mut(e)
                    {
                        *hidden = true;
                    }
                }
                for (emote, hidden) in rich.emotes.iter_mut().zip(hidden) {
                    if !hidden {
                        emote.picture = images.emote_ready(&emote.url);
                        emote_wants.push((i, emote.url.clone()));
                    }
                }
            }
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
            // A voice or audio message: its first line and its waveform change
            // while it loads or plays (they are drawn over, see `draw`).
            let sound = sound_rows(message, &rich);
            let head_rows = wrapped.iter().filter(|row| row.line == 0).count() as u16;
            let mut wave_seen = false;
            let play_style = Style {
                fg: theme.style(Role::Link).fg,
                ..Style::new()
            }
            .add_modifier(Modifier::BOLD);
            // The block the previous row belonged to, to find the first row of each.
            let mut in_block = None;
            for (n, row) in wrapped.drain(..).enumerate() {
                let first_of = row
                    .block
                    .filter(|b| in_block != Some(*b))
                    .filter(|b| rich.code_blocks.get(*b).is_some_and(|c| !c.text.is_empty()));
                in_block = row.block;
                let audio = sound.and_then(|waveform| match row.line {
                    0 => Some(AudioPart {
                        head: true,
                        first: n == 0,
                        nth: n as u16,
                        of: head_rows,
                        marker: 0,
                    }),
                    1 if waveform => Some(AudioPart {
                        head: false,
                        first: !std::mem::replace(&mut wave_seen, true),
                        nth: 0,
                        of: 1,
                        marker: 0,
                    }),
                    _ => None,
                });
                let mut spans = vec![Span::raw(INDENT)];
                let mut segment_hits = Vec::new();
                let mut spots = Vec::new();
                let mut col = INDENT.len();
                let play_start = col as u16;
                if !row.prefix.is_empty() {
                    col += width(&row.prefix);
                    spans.push(Span::styled(row.prefix, theme.style(Role::Muted)));
                }
                for (seg_idx, segment) in row.segments.into_iter().enumerate() {
                    let segment_width = width(&segment.text);
                    if let Some(emote) = segment.emote_box().and_then(|e| rich.emotes.get(e)) {
                        spots.push(EmoteSpot {
                            col: col as u16,
                            url: emote.url.clone(),
                        });
                    }
                    let (text, style) = styled(&segment, body_style, theme, reveal);
                    if let Some(link) = segment.format.link {
                        segment_hits.push((
                            col as u16,
                            (col + segment_width) as u16,
                            HitId::Link(i, link),
                        ));
                    }
                    col += segment_width;
                    if n == 0
                        && seg_idx == 0
                        && playable
                        && let Some(rest) = text.strip_prefix("▶ ")
                    {
                        spans.push(Span::styled("▶ ", play_style));
                        if !rest.is_empty() {
                            spans.push(Span::styled(rest.to_owned(), style));
                        }
                    } else {
                        spans.push(Span::styled(text, style));
                    }
                }
                let mut hits = Vec::new();
                if n == 0 && playable && (col as u16) > play_start {
                    hits.push((play_start, col as u16, HitId::PlayMedia(i)));
                }
                hits.extend(segment_hits);
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
                    emotes: spots,
                    audio,
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
        }

        // Reaction chips, wrapped onto further rows when they do not fit. One made
        // with a custom emoji shows its picture once it is there.
        let mut spans = vec![Span::raw(INDENT)];
        let mut hits = Vec::new();
        let mut spots = Vec::new();
        let mut col = INDENT.len();
        // Reactions stay on a message that could not be decrypted (they are not
        // encrypted), but like its text they show nothing of their pictures.
        let chip_pictures = images.emote_pictures()
            && !matches!(message.kind, MsgKind::Redacted | MsgKind::Undecryptable)
            && !message
                .sender_id
                .as_deref()
                .is_some_and(|id| emotes.ignored.contains(id));
        for (r, reaction) in message.reactions.iter().enumerate() {
            let key = reaction.key.trim();
            let wanted = chip_pictures && is_mxc_key(key) && is_valid_mxc(key);
            if wanted {
                emote_wants.push((i, key.to_owned()));
            }
            // A chip too wide for the pane is cut short, and then has no room for
            // the picture: it shows the name instead, as much of it as fits.
            let pictured = format!(" {} {} ", rich::EMOTE_BOX, reaction.count);
            let picture = wanted && images.emote_ready(key) && width(&pictured) <= body_width;
            let whole = if picture {
                pictured
            } else {
                format!(
                    " {} {} ",
                    reaction_label(&reaction.key, emotes.catalog),
                    reaction.count
                )
            };
            let chip = truncate(&whole, body_width);
            let chip_width = width(&chip);
            if col + chip_width > max_width && !hits.is_empty() {
                rows.push(Row {
                    line: Line::from(std::mem::replace(&mut spans, vec![Span::raw(INDENT)])),
                    msg: Some(i),
                    source: i,
                    hits: std::mem::take(&mut hits),
                    image: None,
                    emotes: std::mem::take(&mut spots),
                    audio: None,
                });
                col = INDENT.len();
            }
            if picture {
                spots.push(EmoteSpot {
                    col: col as u16 + 1,
                    url: key.to_owned(),
                });
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
                emotes: spots,
                audio: None,
            });
        }
        if let Some(thread) = &message.thread {
            rows.push(thread_row(thread, i, body_width, theme));
        }
    }
    for row in &mut rows[start..] {
        row.source = i;
    }
    cut_short
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
        emotes: Vec::new(),
        audio: None,
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

#[allow(clippy::too_many_arguments)]
fn push_poll_rows(
    rows: &mut Vec<Row>,
    i: usize,
    message: &Message,
    poll: &crate::model::PollInfo,
    body_width: usize,
    max_width: usize,
    theme: &Theme,
    text_style: Style,
    repeated_shield: bool,
) {
    let fg_only = |role: Role| Style {
        fg: theme.style(role).fg,
        ..Style::new()
    };
    let muted = fg_only(Role::Muted);
    let accent = fg_only(Role::StatusKey).add_modifier(Modifier::BOLD);
    let bold_text = text_style.add_modifier(Modifier::BOLD);

    // Question header, wrapped to fit after the icon.
    let q_prefix = "📊 ";
    let q_width = body_width.saturating_sub(width(q_prefix)).max(1);
    let q_lines = wrap(&poll.question, q_width);
    for (idx, line) in q_lines.into_iter().enumerate() {
        let mut spans = vec![Span::raw(INDENT)];
        if idx == 0 {
            spans.push(Span::styled(q_prefix, accent));
        } else {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(line, bold_text));
        rows.push(Row::plain(Line::from(spans), Some(i)));
    }

    // Answer rows.
    let shows_counts = poll.shows_counts();
    let winner_max = poll.winner_max();
    let clickable = !poll.ended && message.state == SendState::Sent;
    for (a_idx, answer) in poll.answers.iter().enumerate() {
        let is_winner = winner_max > 0 && answer.votes == winner_max;
        let mark = match (poll.is_multi(), answer.own) {
            (false, true) => "● ",
            (false, false) => "○ ",
            (true, true) => "[✓] ",
            (true, false) => "[ ] ",
        };
        let mark_style = if answer.own || is_winner {
            accent
        } else {
            muted
        };
        let text_role = if is_winner || answer.own {
            bold_text
        } else {
            text_style
        };

        let mut spans = vec![Span::raw(INDENT), Span::styled(mark, mark_style)];
        let mut used = width(mark);
        if shows_counts {
            let pct = if poll.total_voters > 0 {
                ((u64::from(answer.votes) * 100) / u64::from(poll.total_voters)).min(100)
            } else {
                0
            };
            let win_tag = if is_winner { " ★" } else { "" };
            let count_str = format!(" {} ({pct}%){win_tag}", answer.votes);
            let bar_w = if body_width >= 30 {
                (body_width / 4).clamp(4, 12)
            } else {
                0
            };
            let bar_total_w = if bar_w > 0 { bar_w + 1 } else { 0 };
            let label_budget = body_width
                .saturating_sub(used + bar_total_w + width(&count_str))
                .max(1);
            let label = truncate(&answer.text, label_budget);
            used += width(&label);
            spans.push(Span::styled(label, text_role));
            if bar_w > 0 {
                let mut filled = if poll.total_voters > 0 {
                    ((u64::from(answer.votes) * bar_w as u64 + u64::from(poll.total_voters) / 2)
                        / u64::from(poll.total_voters))
                    .min(bar_w as u64) as usize
                } else {
                    0
                };
                if answer.votes > 0 && filled == 0 {
                    filled = 1;
                }
                let bar = format!(
                    " {}{}",
                    "█".repeat(filled),
                    "░".repeat(bar_w.saturating_sub(filled))
                );
                used += width(&bar);
                spans.push(Span::styled(
                    bar,
                    if is_winner || answer.own {
                        accent
                    } else {
                        fg_only(Role::Link)
                    },
                ));
            }
            used += width(&count_str);
            spans.push(Span::styled(
                count_str,
                if is_winner { accent } else { muted },
            ));
        } else {
            let label = truncate(&answer.text, body_width.saturating_sub(used).max(1));
            used += width(&label);
            spans.push(Span::styled(label, text_role));
        }

        let start_col = INDENT.len() as u16;
        let end_col = (INDENT.len() + used).min(max_width) as u16;
        let hits = if clickable && end_col > start_col {
            vec![(start_col, end_col, HitId::PollAnswer(i, a_idx))]
        } else {
            Vec::new()
        };
        rows.push(Row {
            line: Line::from(spans),
            msg: Some(i),
            source: i,
            hits,
            image: None,
            emotes: Vec::new(),
            audio: None,
        });
    }

    // Footer metadata line: visibility, choice cap, voter count (when disclosed/ended), and marker.
    let mut meta = Vec::new();
    if poll.ended {
        meta.push("ended".to_owned());
    }
    meta.push(if poll.disclosed {
        "disclosed".to_owned()
    } else {
        "undisclosed".to_owned()
    });
    if poll.is_multi() {
        meta.push(format!("up to {}", poll.effective_max()));
    } else {
        meta.push("single choice".to_owned());
    }
    if shows_counts {
        meta.push(format!(
            "{} voter{}",
            poll.total_voters,
            if poll.total_voters == 1 { "" } else { "s" }
        ));
    } else {
        meta.push("results hidden until ended".to_owned());
    }
    let meta_text = truncate(&meta.join(" · "), body_width);
    let mut footer_spans = vec![Span::raw(INDENT), Span::styled(meta_text.clone(), muted)];
    let col = INDENT.len() + width(&meta_text);
    match marker(message, repeated_shield) {
        Some((label, role)) if col + 1 + width(&label) <= max_width => {
            footer_spans.push(Span::raw(" "));
            footer_spans.push(Span::styled(label, fg_only(role)));
            rows.push(Row::plain(Line::from(footer_spans), Some(i)));
        }
        Some((label, role)) => {
            rows.push(Row::plain(Line::from(footer_spans), Some(i)));
            rows.push(Row::plain(
                Line::from(vec![
                    Span::raw(INDENT),
                    Span::styled(truncate(&label, body_width), fg_only(role)),
                ]),
                Some(i),
            ));
        }
        None => {
            rows.push(Row::plain(Line::from(footer_spans), Some(i)));
        }
    }
}

/// Everything the rows depend on: they are laid out again when any of it changes.
#[derive(PartialEq)]
#[cfg_attr(test, derive(Default))]
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
    /// changes, or who is ignored (which decides about custom emoji pictures too).
    previews: (u64, LinkPreviews, bool, u64),
    /// Changes when image packs arrive: reactions show their shortcodes.
    emotes: u64,
    /// Changes when a custom emoji's picture arrives or goes: two cells or its name.
    emote_pictures: u64,
    /// Changes when a transcript or translation comes, goes, is hidden or shown.
    notes: u64,
}

/// Hand-written: the room's id and the theme's name stay out of logs.
impl std::fmt::Debug for RowKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowKey")
            .field("messages", &self.messages)
            .field("width", &self.width)
            .field("revealed", &self.revealed)
            .field("images", &self.images)
            .field("previews", &self.previews)
            .field("emotes", &self.emotes)
            .field("emote_pictures", &self.emote_pictures)
            .field("notes", &self.notes)
            .finish_non_exhaustive()
    }
}

/// The message the mouse is on, by whatever of it.
fn hovered_message(hover: Option<HitId>) -> Option<usize> {
    message_target(hover?, 0).0
}

/// The message that `id` is a part of, if it is one of a message; and the same part of
/// the message at `to`.
fn message_target(id: HitId, to: usize) -> (Option<usize>, HitId) {
    match id {
        HitId::Message(m) => (Some(m), HitId::Message(to)),
        HitId::Reaction(m, r) => (Some(m), HitId::Reaction(to, r)),
        HitId::Link(m, link) => (Some(m), HitId::Link(to, link)),
        HitId::Image(m) => (Some(m), HitId::Image(to)),
        HitId::PlayMedia(m) => (Some(m), HitId::PlayMedia(to)),
        HitId::PollAnswer(m, answer) => (Some(m), HitId::PollAnswer(to, answer)),
        HitId::Thread(m) => (Some(m), HitId::Thread(to)),
        HitId::CopyCode(m, block) => (Some(m), HitId::CopyCode(to, block)),
        HitId::ReplyQuote(m) => (Some(m), HitId::ReplyQuote(to)),
        HitId::Sender(m) => (Some(m), HitId::Sender(to)),
        HitId::NoteButton(m, kind) => (Some(m), HitId::NoteButton(to, kind)),
        HitId::Pane(_)
        | HitId::Room(_)
        | HitId::BarRoom(_)
        | HitId::RoomFilter
        | HitId::ComposerText
        | HitId::Field(_)
        | HitId::Input(_)
        | HitId::Emoji(_)
        | HitId::Setting(..)
        | HitId::Splitter
        | HitId::Button(_)
        | HitId::Do(_)
        | HitId::Backdrop
        | HitId::Overlay
        | HitId::Attachment(_)
        | HitId::AttachmentPlay(_)
        | HitId::Palette(_)
        | HitId::GlobalSearch(_)
        | HitId::Complete(_)
        | HitId::Menu(_)
        | HitId::KeybindRow(_)
        | HitId::KeybindBtn(_)
        | HitId::IgnoredRow(_)
        | HitId::IgnoredUnignore(_)
        | HitId::ReportIgnoreToggle
        | HitId::ReportSubmit
        | HitId::UserSearchRow(_)
        | HitId::UserSearchInfo(_)
        | HitId::UserInfoRow(_)
        | HitId::UserInfoCopy
        | HitId::UserInfoBanSubmit
        | HitId::UserInfoRoleOption(_)
        | HitId::OwnProfileSlot(_)
        | HitId::PinnedRow(_)
        | HitId::PinnedUnpin(_)
        | HitId::ReasonConfirmBtn(_)
        | HitId::Toast
        | HitId::PollFormSlot(_)
        | HitId::PollFormRemove(_)
        | HitId::PollFormMaxStep(_)
        | HitId::PollVoteAnswer(_)
        | HitId::PollVoteBtn(_)
        | HitId::StickerRow(_)
        | HitId::DetailsFlip
        | HitId::DetailsCopy
        | HitId::DoctorCopy
        | HitId::ThreadRow(_)
        | HitId::ThreadsMore
        | HitId::ThreadsMine
        | HitId::Collect(_)
        | HitId::KeywordRow(_)
        | HitId::KeywordRemove(_)
        | HitId::SavedRow(_)
        | HitId::SavedRemind(_)
        | HitId::SavedRemove(_)
        | HitId::SavedGap
        | HitId::ReminderChoice(_)
        | HitId::ReminderDue => (None, id),
    }
}

/// The laid-out rows of the open room, reused between frames while nothing that
/// affects them has changed.
pub struct RowCache {
    key: RowKey,
    rows: Vec<Row>,
    /// The links to ask the homeserver about (see `Layout::wants`).
    wants: Vec<(usize, String)>,
    /// The custom emoji pictures to fetch (see `Layout::emote_wants`).
    emote_wants: Vec<(usize, String)>,
    /// What the rows of each message were laid out from: when anything changes, those
    /// of the messages that are as they were are kept.
    memo: memo::Memo,
    /// What was at the bottom of the view when it was last drawn scrolled up.
    anchor: Option<Anchor>,
}

/// Hand-written: the rows are the room's messages, the key names the room, and the
/// wanted ones are addresses from messages; how many there are is shown.
impl std::fmt::Debug for RowCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowCache")
            .field("key", &self.key)
            .field("rows", &self.rows.len())
            .field("wants", &self.wants.len())
            .field("emote_wants", &self.emote_wants.len())
            .field("memo", &self.memo)
            .field("anchor", &self.anchor)
            .finish_non_exhaustive()
    }
}

/// Pins the view to a message while the rows around it change: new messages below,
/// a picture whose size becomes known, another width.
#[derive(Clone)]
struct Anchor {
    /// The message the bottom row of the view belonged to.
    id: String,
    /// Rows of that message below the bottom row of the view.
    below: usize,
    /// The scroll position this was recorded at.
    scroll: usize,
}

/// Hand-written: which message it is stays out of logs.
impl std::fmt::Debug for Anchor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Anchor")
            .field("below", &self.below)
            .field("scroll", &self.scroll)
            .finish_non_exhaustive()
    }
}

/// Draws the timeline into `area`. `popup`: where the completion popup goes over it
/// later in this frame.
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, popup: Option<Rect>) {
    // (A borrow of the list of rooms alone: the rest of the app is written to below.)
    let room = match app.open_index() {
        Some(open) => app.rooms.get(open),
        None => None,
    };
    let Some(room) = room else {
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
            // The people themselves, not how many: one may take another's place.
            {
                let mut hasher = DefaultHasher::new();
                app.ignored_users.hash(&mut hasher);
                hasher.finish()
            },
        ),
        emotes: app.emotes.rev,
        emote_pictures: app.images.emote_rev,
        notes: app.notes().rev,
    };
    let previous = app.row_cache.take();
    let mut cache = match previous {
        Some(cache) if cache.key == key => cache,
        mut previous => {
            let anchor = previous
                .as_mut()
                .filter(|old| old.key.room == key.room)
                .and_then(|old| old.anchor.take());
            let view = PreviewView {
                store: &app.previews,
                mode: app.link_previews,
                encrypted: room.encrypted,
                ignored: &app.ignored_users,
            };
            let emote_view = EmoteView {
                catalog: &app.emotes.catalog,
                ignored: &app.ignored_users,
            };
            let lay = Lay::new(
                content.width as usize,
                theme,
                revealed,
                &app.images,
                Some(&view),
                &emote_view,
            );
            // Transcripts and translations go under their messages.
            let notes = memo::NoteView {
                notes: app.notes(),
                current: &|message, kind, note| app.note_current(message, kind, note),
            };
            // Only the messages that changed are laid out again (see `memo`).
            let cache = memo::lay_out(
                room,
                &lay,
                &notes,
                app.theme_rev,
                app.read_markers,
                key,
                previous,
            );
            let rows = &cache.rows;
            // Reading older messages (or paging forwards in a detached view): keep the
            // same bottom row on screen when messages arrive below or change height.
            if let Some(anchor) = anchor
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
            cache
        }
    };
    let rows = &cache.rows;
    // The sound that loads or plays, where it is in this room: rows of its message look
    // different meanwhile, without being laid out again.
    let sound = app.audio_in(room);
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
    let hovered = hovered_message(app.hover);
    // The message whose buttons are still to be put on a row with room for them.
    let mut waiting: Option<(usize, notes::Buttons)> = None;
    for (n, row) in rows.iter().skip(top).take(height).enumerate() {
        let rect = Rect::new(content.x, content.y + pad + n as u16, content.width, 1);
        if let Some(msg) = row.msg {
            if selected == Some(msg) {
                frame
                    .buffer_mut()
                    .set_style(rect, theme.style(Role::MessageSelected));
            } else if hovered == Some(msg) {
                frame.buffer_mut().set_style(rect, theme.style(Role::Hover));
            }
            app.hits.add(rect, HitId::Message(msg));
        }
        // A read marker at the end of the row stays where it is, and its gap with it.
        let kept = row.audio.map_or(0, |part| part.marker).min(rect.width);
        let left = rect.width - kept;
        let room_left = usize::from(left).saturating_sub(if kept > 0 { 2 } else { 0 });
        let sounding = row
            .audio
            .zip(sound)
            .filter(|(_, (at, _))| *at == row.source)
            .and_then(|(part, (at, view))| {
                let message = room.messages.get(at)?;
                let widths = (room_left, usize::from(rect.width));
                sound_row(part, view, message, widths, theme, &mut app.images)
            });
        match &sounding {
            Some(line) => {
                frame.render_widget(&row.line, rect);
                // What the row said is gone from under the sound, with its underlines
                // (a link in a file name); the row's colours stay.
                let buffer = frame.buffer_mut();
                for x in rect.x..rect.x + left {
                    if let Some(cell) = buffer.cell_mut((x, rect.y)) {
                        cell.set_symbol(" ");
                        cell.modifier.remove(Modifier::UNDERLINED);
                    }
                }
                // Over all of what the row said, and no further than its marker.
                let pad = usize::from(left).saturating_sub(line.width());
                let mut over = line.clone();
                over.spans.push(Span::raw(" ".repeat(pad)));
                frame.render_widget(&over, Rect::new(rect.x, rect.y, left, 1));
            }
            None => frame.render_widget(&row.line, rect),
        }
        for (start, end, id) in &row.hits {
            // Under a sound only the marker's part of the row is still what was laid
            // out: a click elsewhere must not reach what is no longer shown.
            if sounding.is_some() && *start < left {
                continue;
            }
            let start = (*start).min(rect.width);
            let end = (*end).min(rect.width);
            app.hits
                .add(Rect::new(rect.x + start, rect.y, end - start, 1), *id);
        }
        // The buttons that transcribe and translate, at the free end of the first row
        // of their message that has room for them (see `notes`).
        if let Some(msg) = row.msg {
            let lit = selected == Some(msg) || hovered == Some(msg);
            if waiting.as_ref().is_none_or(|(of, _)| *of != msg) {
                let buttons = room
                    .messages
                    .get(msg)
                    .map(|message| notes::buttons(app, message, lit))
                    .unwrap_or_default();
                waiting = Some((msg, buttons));
            }
            let free = row.image.is_none() && sounding.is_none();
            // What the row says, and where its read marker begins if it has one.
            let (taken, end) = notes::free_part(&row.line, usize::from(rect.width));
            if let Some((_, buttons)) = &mut waiting
                && free
                && let Some(placed) = notes::place(buttons, taken, end)
            {
                for (col, label, kind) in placed {
                    let spot = Rect::new(rect.x + col, rect.y, width(&label) as u16, 1);
                    let style =
                        notes::button_style(theme, lit, notes::is_hovered(app.hover, msg, kind));
                    frame.render_widget(Line::from(Span::styled(label, style)), spot);
                    app.hits.add(spot, HitId::NoteButton(msg, kind));
                }
                buttons.clear();
            }
        }
        // All of the row that took the first line's place stops the sound.
        if let (Some(part), Some(line)) = (row.audio, &sounding)
            && part.head
            && part.first
        {
            let start = (INDENT.len() as u16).min(rect.width);
            let end = (line.width() as u16).clamp(start, rect.width);
            app.hits.add(
                Rect::new(rect.x + start, rect.y, end - start, 1),
                HitId::PlayMedia(row.source),
            );
        }
    }

    // Pictures are painted over the rows kept free for them; what is scrolled out of
    // the view is left out. Nothing new is sent to the terminal where something is
    // drawn over it later in the frame (a popup, a corner pill, the completion popup):
    // such a transmission never arrives, and the other copies of that picture on
    // screen would stay empty (see `ImageStore::verify`). A picture already sent
    // shows around them.
    let covered = app.modal.is_some() || app.help_open || app.images.viewer_open();
    let pills = corner_pills(app, content, scroll);
    let overlays: Vec<Rect> = pills.iter().map(|pill| pill.0).chain(popup).collect();
    // A picture's data travels in its first cell.
    let under_overlay = |x: u16, y: u16| {
        overlays
            .iter()
            .any(|overlay| overlay.contains(Position::new(x, y)))
    };
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
        if let Some(msg) = room.messages.get(source)
            && let Some(info) = msg.image.as_ref()
        {
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
                play_mark: msg.is_playable() && msg.state != SendState::Failed,
                covered: covered || under_overlay(rect.x, rect.y),
            };
            super::image::draw_inline(frame, &mut app.images, theme, inline);
        }
        n += count;
    }
    // Custom emoji pictures, over the two cells kept for each. Until one can be drawn
    // (it is held back where it could be painted over, see `ImageStore::inline`) the
    // cells stay empty. The same emoji is often on screen more than once: its data
    // travels with the first copy that nothing is drawn over.
    for (n, row) in rows.iter().enumerate().take(last_visible).skip(top) {
        let y = content.y + pad + (n - top) as u16;
        for spot in &row.emotes {
            let x = content.x + spot.col;
            if x + EMOTE_CELLS.0 <= content.right() {
                let rect = Rect::new(x, y, EMOTE_CELLS.0, EMOTE_CELLS.1);
                let covered = covered || under_overlay(x, y);
                super::image::draw_emote(frame, &mut app.images, theme, &spot.url, rect, covered);
            }
        }
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
    // So are the custom emoji pictures of the messages on screen, a few at a time (see
    // `ImageStore::request_emotes`); each one that arrives lays the rows out again.
    if !cache.emote_wants.is_empty()
        && let (Some(first), Some(last)) = (rows.get(top), rows.get(last_visible.wrapping_sub(1)))
    {
        let on_screen = first.source..=last.source;
        app.images.request_emotes(
            cache
                .emote_wants
                .iter()
                .filter(|(msg, _)| on_screen.contains(msg))
                .map(|(_, url)| url.as_str()),
        );
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

    for (rect, label, hit) in pills {
        frame.render_widget(
            Line::from(Span::styled(label, theme.style(Role::Pill))),
            rect,
        );
        if let Some(hit) = hit {
            app.hits.add(rect, hit);
        }
    }
    cache.anchor = anchor;
    app.row_cache = Some(cache);
}

/// The pills in the corners of the view: what is being loaded, and the way back to the
/// newest message, with what a click on one does. Known before the pictures are drawn,
/// which must not send anything to the terminal under them.
fn corner_pills(
    app: &App,
    content: Rect,
    scroll: usize,
) -> Vec<(Rect, &'static str, Option<HitId>)> {
    let mut pills = Vec::new();
    if app.timeline.loading_older && content.width > 24 {
        let label = " loading older messages… ";
        let rect = Rect::new(content.x + 1, content.y, width(label) as u16, 1);
        pills.push((rect, label, None));
    }
    if app.timeline.loading_newer && content.width > 26 && (scroll == 0 || content.width > 38) {
        let label = " loading newer messages… ";
        let rect = Rect::new(content.x + 1, content.bottom() - 1, width(label) as u16, 1);
        pills.push((rect, label, None));
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
            pills.push((rect, label, Some(HitId::Button(Button::JumpToNewest))));
        }
    }
    pills
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
    fn custom_emoji_reactions_show_their_shortcode_never_their_address() {
        use crate::matrix::{CustomEmoji, EmotePack};
        use crate::model::Reaction;
        let emotes = EmoteCatalog {
            packs: vec![EmotePack {
                id: "user".into(),
                name: "Mine".into(),
                emojis: vec![CustomEmoji {
                    shortcode: "blobcat".into(),
                    url: "mxc://example.org/blobcat".into(),
                    body: None,
                    pack_name: "Mine".into(),
                }],
                stickers: Vec::new(),
            }],
        };
        assert_eq!(reaction_label("👍", &emotes), "👍");
        assert_eq!(
            reaction_label("mxc://example.org/blobcat", &emotes),
            ":blobcat:"
        );
        assert_eq!(
            reaction_label(" MXC://example.org/other", &emotes),
            rich::EMOTICON_PLACEHOLDER
        );

        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let mut room = demo_rooms().remove(0);
        room.messages = vec![Message {
            body: "hello".into(),
            reactions: [
                "mxc://example.org/blobcat",
                "mxc://example.org/unknown",
                "🐀",
            ]
            .into_iter()
            .map(|key| Reaction {
                key: key.into(),
                count: 2,
                own: false,
            })
            .collect(),
            ..Message::default()
        }];
        let images = ImageStore::new(Graphics::halfblocks());
        let view = EmoteView {
            catalog: &emotes,
            ignored: &BTreeSet::new(),
        };
        let text: String = build_layout(&room, 60, &theme, None, &images, None, &view)
            .rows
            .iter()
            .map(|row| row.line.to_string())
            .collect();
        assert!(text.contains(" :blobcat: 2 "), "{text}");
        assert!(text.contains(" :?: 2 "), "{text}");
        assert!(text.contains(" 🐀 2 "), "{text}");
        assert!(!text.contains("mxc"), "{text}");
    }

    const BLOB: &str = "mxc://example.org/blob";
    const PARTY: &str = "mxc://example.org/party";

    /// A message from `sender` with a custom emoji in its text (inside `<span>` and
    /// `</span>`, if given), and a reaction made with another.
    fn emoji_message(sender: &str, around: (&str, &str)) -> Message {
        use crate::model::Reaction;
        Message {
            body: "hi :blob: there".into(),
            html: Some(format!(
                "hi {}<img data-mx-emoticon src=\"{BLOB}\" alt=\":blob:\">{} there",
                around.0, around.1
            )),
            sender_id: Some(sender.into()),
            reactions: vec![Reaction {
                key: PARTY.into(),
                count: 2,
                own: false,
            }],
            ..Message::default()
        }
    }

    fn emoji_layout(
        message: Message,
        images: &ImageStore,
        ignored: &BTreeSet<String>,
        revealed: Option<usize>,
    ) -> (Layout, String) {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let mut room = demo_rooms().remove(0);
        room.messages = vec![message];
        let view = EmoteView {
            catalog: &EmoteCatalog::default(),
            ignored,
        };
        let layout = build_layout(&room, 60, &theme, revealed, images, None, &view);
        let text = layout
            .rows
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        (layout, text)
    }

    /// Where pictures are painted: row, column and address.
    fn spots(layout: &Layout) -> Vec<(usize, u16, &str)> {
        layout
            .rows
            .iter()
            .enumerate()
            .flat_map(|(n, row)| row.emotes.iter().map(move |s| (n, s.col, s.url.as_str())))
            .collect()
    }

    #[test]
    fn custom_emoji_take_two_cells_once_their_picture_is_there() {
        use crate::images::testing::{decoded, kitty};
        let nobody = BTreeSet::new();
        let message = || emoji_message("@alice:example.org", ("", ""));
        let mut images = ImageStore::new(kitty());

        // Not there yet: their names, as before, and both are wanted.
        let (layout, text) = emoji_layout(message(), &images, &nobody, None);
        assert!(text.contains("  hi :blob: there"), "{text}");
        assert!(text.contains("   :?: 2 "), "{text}");
        assert_eq!(
            layout.emote_wants,
            [(0, BLOB.to_owned()), (0, PARTY.to_owned())]
        );
        assert!(spots(&layout).is_empty());

        // There: two cells each, in the text and on the reaction, painted over.
        images.begin_frame();
        images.request_emotes([BLOB, PARTY]);
        images.on_emote(BLOB, Ok(decoded(64, 64)));
        images.on_emote(PARTY, Ok(decoded(64, 64)));
        let (layout, text) = emoji_layout(message(), &images, &nobody, None);
        let lines: Vec<&str> = text.lines().collect();
        let row = lines.iter().position(|l| l.contains("hi")).unwrap();
        let chips = lines.iter().position(|l| l.contains(" 2 ")).unwrap();
        assert_eq!(lines[row].trim_end(), "  hi    there");
        assert_eq!(lines[chips].trim_end(), "      2");
        assert_eq!(spots(&layout), [(row, 5, BLOB), (chips, 3, PARTY)]);
        assert!(!text.contains("blob") && !text.contains(":?:"), "{text}");

        // One that failed keeps its name.
        let mut failed = ImageStore::new(kitty());
        failed.begin_frame();
        failed.request_emotes([BLOB]);
        failed.on_emote(BLOB, Err("could not be loaded".into()));
        let (layout, text) = emoji_layout(message(), &failed, &nobody, None);
        assert!(text.contains("hi :blob: there"), "{text}");
        assert!(spots(&layout).is_empty());

        // Without kitty's protocol, or with pictures off: names, and nothing wanted.
        for graphics in [Graphics::halfblocks(), Graphics::off()] {
            let images = ImageStore::new(graphics);
            let (layout, text) = emoji_layout(message(), &images, &nobody, None);
            assert!(text.contains("hi :blob: there"), "{text}");
            assert!(layout.emote_wants.is_empty());
            assert!(spots(&layout).is_empty());
        }
    }

    #[test]
    fn a_reaction_chip_too_wide_for_the_pane_shows_its_name_cut_short() {
        use crate::images::testing::{decoded, kitty};
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let mut images = ImageStore::new(kitty());
        images.begin_frame();
        images.request_emotes([PARTY]);
        images.on_emote(PARTY, Ok(decoded(64, 64)));
        let mut room = demo_rooms().remove(0);
        let mut message = emoji_message("@alice:example.org", ("", ""));
        message.reactions[0].count = 1_234_567_890;
        room.messages = vec![message];
        let nobody = BTreeSet::new();
        let view = EmoteView {
            catalog: &EmoteCatalog::default(),
            ignored: &nobody,
        };
        let chip = |layout: &Layout| -> (String, usize) {
            let row = layout
                .rows
                .iter()
                .find(|row| row.hits.iter().any(|hit| hit.2 == HitId::Reaction(0, 0)))
                .expect("a chip");
            (row.line.to_string(), row.emotes.len())
        };
        // Twelve columns for the chip; with the picture it would take fifteen.
        let (text, pictures) = chip(&build_layout(&room, 14, &theme, None, &images, None, &view));
        assert!(text.trim_start().starts_with(":?: 123"), "{text:?}");
        assert_eq!(pictures, 0);
        // With room enough, the picture.
        let (text, pictures) = chip(&build_layout(&room, 17, &theme, None, &images, None, &view));
        assert_eq!(text.trim(), "1234567890", "{text:?}");
        assert_eq!(pictures, 1);
    }

    #[test]
    fn hidden_messages_and_ignored_people_fetch_no_emoji_pictures() {
        use crate::images::testing::kitty;
        let images = ImageStore::new(kitty());
        let nobody = BTreeSet::new();
        let alice = "@alice:example.org";

        // Under a spoiler: only once the message is uncovered. The reaction is not
        // covered by it.
        let spoiler = || emoji_message(alice, ("<span data-mx-spoiler>", "</span>"));
        let (layout, text) = emoji_layout(spoiler(), &images, &nobody, None);
        assert_eq!(layout.emote_wants, [(0, PARTY.to_owned())]);
        assert!(!text.contains("blob"), "{text}");
        let (layout, _) = emoji_layout(spoiler(), &images, &nobody, Some(0));
        assert_eq!(
            layout.emote_wants,
            [(0, BLOB.to_owned()), (0, PARTY.to_owned())]
        );

        // Deleted, or not decrypted (its reactions are still there): nothing.
        for kind in [MsgKind::Redacted, MsgKind::Undecryptable] {
            let hidden = Message {
                kind,
                ..emoji_message(alice, ("", ""))
            };
            assert!(
                emoji_layout(hidden, &images, &nobody, None)
                    .0
                    .emote_wants
                    .is_empty(),
                "{kind:?}"
            );
        }

        // From someone ignored: nothing either.
        let ignored = BTreeSet::from([alice.to_owned()]);
        let (layout, _) = emoji_layout(emoji_message(alice, ("", "")), &images, &ignored, None);
        assert!(layout.emote_wants.is_empty());
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

    /// A message by `sender`, a name, from the account `id`.
    fn by(id: &str, sender: &str, body: &str) -> Message {
        Message {
            id: body.into(),
            sender: sender.into(),
            sender_id: Some(id.into()),
            time: "12:00".into(),
            day: "Today".into(),
            body: body.into(),
            ..Message::default()
        }
    }

    /// The name each sender's header shows, and its colour.
    fn headers(rows: &[Row]) -> Vec<(String, Option<ratatui::style::Color>)> {
        rows.iter()
            .filter(|row| row.line.spans.first().is_some_and(|s| s.content == "● "))
            .map(|row| {
                let name = &row.line.spans[1];
                (name.content.to_string(), name.style.fg)
            })
            .collect()
    }

    /// Two accounts on two servers with the same name (no display names: the first
    /// part of their ids, as the user's own two had) are two people: a header each,
    /// each in the colour of their id, a shield said again for the other.
    #[test]
    fn senders_who_share_a_name_are_told_apart() {
        use crate::model::Shield;
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        let here = "@samuelsson:home1.test";
        let there = "@samuelsson:home2.test";
        // Coloured by their ids, which have two colours in this theme.
        assert_ne!(theme.user_color(here), theme.user_color(there));
        let (one, two) = (Some(theme.user_color(here)), Some(theme.user_color(there)));
        let mut room = demo_rooms().remove(0);
        for name in ["samuelsson", "Sammy"] {
            room.messages = vec![
                by(here, name, "from one"),
                by(here, name, "from one again"),
                by(there, name, "from the other"),
            ];
            let rows = build_rows(&room, 60, &theme, None, &images);
            assert_eq!(
                headers(&rows),
                [(name.to_owned(), one), (name.to_owned(), two)],
                "{name}"
            );
            // A doubt about the sender is said for each of them.
            for message in &mut room.messages {
                message.shield = Some(Shield::SenderMismatch);
            }
            let text: Vec<String> = build_rows(&room, 60, &theme, None, &images)
                .iter()
                .map(|row| row.line.to_string())
                .collect();
            let label = Shield::SenderMismatch.label();
            let said: Vec<usize> = (0..text.len())
                .filter(|at| text[*at].contains(label))
                .collect();
            assert_eq!(said.len(), 2, "{text:#?}");
            let other = text.iter().position(|row| row.contains("from the other"));
            assert!(
                other.is_some_and(|other| said[0] < other && other <= said[1]),
                "{text:#?}"
            );

            // The SDK says the name is ambiguous: each is named with their id, in a
            // header and in an action line.
            for message in &mut room.messages {
                message.sender_ambiguous = true;
            }
            room.messages[2].kind = MsgKind::Emote;
            let rows = build_rows(&room, 60, &theme, None, &images);
            assert_eq!(
                headers(&rows),
                [(format!("{name} (@samuelsson:home1.test)"), one)],
                "{name}"
            );
            let text: Vec<String> = rows.iter().map(|row| row.line.to_string()).collect();
            let action = format!("* {name} (@samuelsson:home2.test) from the other");
            assert!(text.iter().any(|row| row.contains(&action)), "{text:#?}");
            // Narrower, the time goes first, then the end of the name; nothing is wider
            // than the pane.
            for max in [10, 20, 30, 40] {
                for row in build_rows(&room, max, &theme, None, &images) {
                    assert!(row.line.width() <= max, "{max}: {:?}", row.line);
                }
            }
        }

        // Someone whose name nobody else has is named as before, and two messages of
        // theirs have one header.
        room.messages = vec![
            by("@alice:example.org", "alice", "hi"),
            by("@alice:example.org", "alice", "again"),
        ];
        let rows = build_rows(&room, 60, &theme, None, &images);
        let alice = Some(theme.user_color("@alice:example.org"));
        assert_eq!(headers(&rows), [("alice".to_owned(), alice)]);
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

    /// What a received sticker takes in the timeline: the rows of a small picture and
    /// no text; one line naming it when its picture cannot be read; and the line that
    /// stands for it where pictures are off.
    #[test]
    fn a_sticker_is_a_small_picture_or_one_line_that_names_it() {
        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let mut room = demo_rooms().remove(0);
        let mut sticker = room.messages[0].clone();
        sticker.id = "sticker".into();
        sticker.html = None;
        sticker.file = None;
        sticker.body = "[sticker] Waving crab".to_owned();
        let mut image = crate::images::testing::remote_image("mxc://example.org/crab", None);
        image.name = "Waving crab".to_owned();
        image.sticker = true;
        sticker.image = Some(image.clone());
        room.messages = vec![sticker];
        let rows_of = |images: &ImageStore| {
            let rows = build_rows(&room, 80, &theme, None, images);
            let pictures = rows.iter().filter(|row| row.image.is_some()).count();
            let text: Vec<String> = rows.iter().map(|row| row.line.to_string()).collect();
            (pictures, text)
        };
        let named = |text: &[String]| text.iter().any(|line| line.contains("Waving crab"));

        // Drawn, as coloured blocks or by the kitty protocol alike: the rows of a
        // sticker's box, a click on which opens it, and no line of text for it.
        for graphics in [Graphics::halfblocks(), crate::images::testing::kitty()] {
            let mut images = ImageStore::new(graphics);
            let (pictures, text) = rows_of(&images);
            assert_eq!(pictures, 7, "{text:?}");
            assert!(!named(&text), "{text:?}");
            // Once it is there, its own size is what counts.
            images.begin_frame();
            images.request(&image);
            images.on_loaded(
                &image.key,
                false,
                Ok(crate::images::testing::decoded(64, 64)),
            );
            let (pictures, text) = rows_of(&images);
            assert_eq!(pictures, 4, "{text:?}");
            assert!(!named(&text), "{text:?}");
        }

        // A picture that cannot be read: no rows are kept for it, one line says so.
        let mut images = ImageStore::new(Graphics::halfblocks());
        images.begin_frame();
        images.request(&image);
        images.on_loaded(
            &image.key,
            false,
            Err("not a picture this app can read".to_owned()),
        );
        let (pictures, text) = rows_of(&images);
        assert_eq!(pictures, 0);
        assert!(
            text.iter()
                .any(|line| line.contains("🖼  Waving crab — not a picture this app can read")),
            "{text:?}"
        );
        assert!(
            !text.iter().any(|line| line.contains("[sticker]")),
            "{text:?}"
        );

        // Pictures off: the line that stands for it.
        let (pictures, text) = rows_of(&ImageStore::new(Graphics::off()));
        assert_eq!(pictures, 0);
        assert!(
            text.iter()
                .any(|line| line.contains("[sticker] Waving crab")),
            "{text:?}"
        );
    }

    #[test]
    fn playable_media_and_location_rows() {
        use matrix_sdk::ruma::events::room::MediaSource;
        use matrix_sdk::ruma::mxc_uri;

        let theme = Theme::builtin(DEFAULT_THEME).unwrap();
        let images = ImageStore::new(Graphics::halfblocks());
        let mut room = demo_rooms().remove(0);

        let mut voice = room.messages[0].clone();
        voice.body = "🎤 note.ogg · 0:12 · 2 kB\n▁▃▆█".to_owned();
        voice.file = Some(crate::model::Attachment {
            key: "mxc://example.org/v".into(),
            name: "note.ogg".into(),
            size: Some(2000),
            kind: crate::model::MediaKind::Voice,
            source: Box::new(MediaSource::Plain(
                mxc_uri!("mxc://example.org/v").to_owned(),
            )),
            audio: None,
        });

        let mut video = room.messages[0].clone();
        video.id = "vid".into();
        video.body = "🎞  clip.mp4 · 1:05 · 4 MB".to_owned();
        video.file = Some(crate::model::Attachment {
            key: "mxc://example.org/vid".into(),
            name: "clip.mp4".into(),
            size: Some(4_000_000),
            kind: crate::model::MediaKind::Video,
            source: Box::new(MediaSource::Plain(
                mxc_uri!("mxc://example.org/vid").to_owned(),
            )),
            audio: None,
        });
        let thumb_key = "mxc://example.org/vthumb";
        video.image = Some(crate::images::testing::remote_image(
            thumb_key,
            Some((320, 180)),
        ));

        let mut loc = room.messages[0].clone();
        loc.id = "loc".into();
        loc.body = "📍 Big Ben · geo:51.5074,-0.1278".to_owned();

        room.messages = vec![voice, video, loc];
        for max in [12, 24, 60] {
            let rows = build_rows(&room, max, &theme, None, &images);
            for row in &rows {
                assert!(row.line.width() <= max, "width {max}: {:?}", row.line);
            }
        }
        let rows = build_rows(&room, 60, &theme, None, &images);
        let text: Vec<String> = rows.iter().map(|r| r.line.to_string()).collect();
        assert!(
            text.iter()
                .any(|l| l.contains("▶ 🎤 note.ogg · 0:12 · 2 kB")),
            "{text:?}"
        );
        assert!(text.iter().any(|l| l.contains("▁▃▆█")), "{text:?}");
        // Video with thumbnail keeps BOTH thumbnail rows and the summary row.
        assert!(rows.iter().any(|r| r.source == 1 && r.image.is_some()));
        assert!(
            text.iter()
                .any(|l| l.contains("▶ 🎞  clip.mp4 · 1:05 · 4 MB")),
            "{text:?}"
        );
        assert!(
            rows.iter()
                .any(|r| r.hits.iter().any(|h| h.2 == HitId::PlayMedia(0)))
        );
        assert!(
            rows.iter()
                .any(|r| r.hits.iter().any(|h| h.2 == HitId::Link(2, 0)))
        );
    }

    #[test]
    fn poll_rows_render_disclosed_undisclosed_and_ended_winners() {
        use crate::model::{PollAnswer, PollInfo};

        let open_undisclosed = Message {
            id: "$p1".into(),
            sender: "alice".into(),
            day: "Today".into(),
            time: "12:00".into(),
            body: "📊 Secret vote?".into(),
            poll: Some(PollInfo {
                question: "Secret vote?".into(),
                disclosed: false,
                max_selections: 2,
                answers: vec![
                    PollAnswer {
                        id: "a1".into(),
                        text: "Option A".into(),
                        votes: 4,
                        own: true,
                    },
                    PollAnswer {
                        id: "a2".into(),
                        text: "Option B".into(),
                        votes: 2,
                        own: false,
                    },
                ],
                total_voters: 5,
                ended: false,
                editable: false,
            }),
            ..Message::default()
        };
        let ended_disclosed = Message {
            id: "$p2".into(),
            sender: "bob".into(),
            day: "Today".into(),
            time: "12:05".into(),
            body: "📊 Best editor?".into(),
            poll: Some(PollInfo {
                question: "Best editor?".into(),
                disclosed: true,
                max_selections: 1,
                answers: vec![
                    PollAnswer {
                        id: "b1".into(),
                        text: "Helix".into(),
                        votes: 3,
                        own: false,
                    },
                    PollAnswer {
                        id: "b2".into(),
                        text: "Neovim".into(),
                        votes: 1,
                        own: true,
                    },
                ],
                total_voters: 4,
                ended: true,
                editable: false,
            }),
            ..Message::default()
        };
        let room = Room {
            messages: vec![open_undisclosed, ended_disclosed],
            ..Room::new("!r:example.org", "test", RoomKind::Room)
        };
        let theme = Theme::builtin(crate::theme::DEFAULT_THEME).unwrap();
        let images = ImageStore::new(crate::images::Graphics::off());
        let rows = build_rows(&room, 72, &theme, None, &images);

        let p1_text: Vec<String> = rows
            .iter()
            .filter(|r| r.source == 0)
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Open undisclosed poll marks own vote (`[✓]`), states `undisclosed · up to 2`,
        // and NEVER leaks vote counts, percentages, bars, or voter counts.
        assert!(p1_text.iter().any(|l| l.contains("📊 Secret vote?")));
        assert!(p1_text.iter().any(|l| l.contains("[✓] Option A")));
        assert!(p1_text.iter().any(|l| l.contains("[ ] Option B")));
        assert!(
            p1_text
                .iter()
                .any(|l| l.contains("undisclosed · up to 2 · results hidden until ended"))
        );
        for line in &p1_text {
            assert!(!line.contains('%'), "leaked pct in {line}");
            assert!(!line.contains('█'), "leaked bar in {line}");
            assert!(!line.contains("voter"), "leaked voters in {line}");
        }
        // Open poll answers are clickable.
        assert!(
            rows.iter()
                .any(|r| r.hits.iter().any(|h| h.2 == HitId::PollAnswer(0, 0)))
        );
        assert!(
            rows.iter()
                .any(|r| r.hits.iter().any(|h| h.2 == HitId::PollAnswer(0, 1)))
        );

        let p2_text: Vec<String> = rows
            .iter()
            .filter(|r| r.source == 1)
            .map(|r| {
                r.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Ended disclosed poll shows counts, bars, winner star (`★`), and is not clickable.
        assert!(
            p2_text
                .iter()
                .any(|l| l.contains("Helix") && l.contains("3 (75%) ★"))
        );
        assert!(
            p2_text
                .iter()
                .any(|l| l.contains("● Neovim") && l.contains("1 (25%)"))
        );
        assert!(
            p2_text
                .iter()
                .any(|l| l.contains("ended · disclosed · single choice · 4 voters"))
        );
        assert!(!rows.iter().any(|r| {
            r.hits
                .iter()
                .any(|h| matches!(h.2, HitId::PollAnswer(1, _)))
        }));
    }
}
