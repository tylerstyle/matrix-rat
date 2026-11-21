//! A stretch of a room's messages as plain text: what an archive is written from
//! ([`crate::archive`]) and what is handed to the user's summary command
//! ([`crate::catchup`]).
//!
//! The worker collects the messages (`crate::matrix::collect`); this file holds the
//! pure parts: what is kept of one message, how far back a collection goes, and the
//! one-message-a-line text for a summary.
//!
//! Of a message this is kept: when, the sender's display name, what kind it is, and
//! its text as it reads now (an edit shows its current text). A reply carries a short
//! reference to what it answers. A deleted or undecryptable message is a marker.
//! Pictures, files, voice messages, audio and video are a line with their kind and
//! file name, never their content. Reactions are kept as key and count: the archive
//! lists them under the message, the summary text leaves them out.

use std::fmt;

use chrono::{DateTime, TimeZone};

use crate::model::{MediaKind, Message, MsgKind};
use crate::text::{sanitize, sanitize_line, truncate};

/// How much of the message a reply answers is quoted, in columns.
const REPLY_QUOTE: usize = 80;
/// The most messages handed to a summary command, and the most text.
pub const SUMMARY_MESSAGES: usize = 300;
pub const SUMMARY_CHARS: usize = 24_000;

/// How far back an archive goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Span {
    /// What the app has of the room already; nothing is asked of the server.
    Loaded,
    /// The last so many days.
    Days(u32),
    /// As far back as the room goes (or as the account may see), up to the most
    /// one archive takes.
    Everything,
}

impl Span {
    pub const ALL: [Span; 4] = [
        Span::Loaded,
        Span::Days(7),
        Span::Days(30),
        Span::Everything,
    ];

    pub fn label(self) -> String {
        match self {
            Span::Loaded => "What is loaded now".to_owned(),
            Span::Days(days) => format!("The last {days} days"),
            Span::Everything => "As far back as it goes".to_owned(),
        }
    }

    /// The oldest moment (milliseconds since 1970) that is still wanted, seen from
    /// `now`; `None` where time is not what bounds it.
    pub fn since(self, now_ms: u64) -> Option<u64> {
        match self {
            Span::Days(days) => Some(now_ms.saturating_sub(u64::from(days) * 24 * 60 * 60 * 1000)),
            Span::Loaded | Span::Everything => None,
        }
    }
}

/// Why a collection holds less than was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// There were more messages than are taken at once; the newest were kept.
    Limit,
    /// Older messages could not be loaded (the server did not answer, or refused).
    LoadFailed,
}

/// What kind of line a message is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// A `/me` action.
    Emote,
    /// A join, a leave, a new room name and the like.
    Event,
    Deleted,
    Undecryptable,
    Poll,
    /// A picture, sticker, file, voice message, audio or video: its kind in a word.
    /// The file name is [`Entry::name`], a caption the entry's text.
    Media(&'static str),
}

/// One message of a collection.
#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    /// Milliseconds since 1970, as the sender's server says.
    pub at: Option<u64>,
    /// The sender's display name, on one line.
    pub sender: String,
    pub kind: Kind,
    /// The text as it reads now; for media the caption (often empty).
    pub text: String,
    /// The file name of media.
    pub name: Option<String>,
    /// Whom and what a reply answers, in a few words.
    pub reply: Option<String>,
    pub edited: bool,
    /// Reaction keys with how many people chose each.
    pub reactions: Vec<(String, u32)>,
}

/// Hand-written: who said what stays out of logs.
impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("kind", &self.kind)
            .field("text_bytes", &self.text.len())
            .field("reply", &self.reply.is_some())
            .field("edited", &self.edited)
            .field("reactions", &self.reactions.len())
            .finish_non_exhaustive()
    }
}

/// What is kept of `message`; `None` for lines that are no messages (the "new" line)
/// and for a message of ours that the server does not have yet.
pub fn entry(message: &Message) -> Option<Entry> {
    if message.kind == MsgKind::ReadMarker || message.state != crate::model::SendState::Sent {
        return None;
    }
    let mut name = None;
    let mut text = message.body.clone();
    let kind = match message.kind {
        MsgKind::System => Kind::Event,
        MsgKind::Redacted => {
            text.clear();
            Kind::Deleted
        }
        MsgKind::Undecryptable => {
            text.clear();
            Kind::Undecryptable
        }
        MsgKind::ReadMarker => return None,
        MsgKind::Emote => Kind::Emote,
        MsgKind::Text => {
            if let Some(file) = &message.file {
                name = Some(file.name.clone());
                text = caption_of(&message.body);
                Kind::Media(match file.kind {
                    MediaKind::File => "File",
                    MediaKind::Audio => "Audio",
                    MediaKind::Voice => "Voice message",
                    MediaKind::Video => "Video",
                    MediaKind::Picture => "Picture",
                })
            } else if let Some(image) = &message.image {
                name = Some(image.name.clone());
                text = image.caption.clone().unwrap_or_default();
                Kind::Media(if image.sticker { "Sticker" } else { "Picture" })
            } else if message.poll.is_some() {
                Kind::Poll
            } else {
                Kind::Text
            }
        }
    };
    let reply = message.reply_to.as_ref().map(|(sender, quoted)| {
        let quoted = truncate(sanitize_line(quoted).trim(), REPLY_QUOTE);
        match sender.trim() {
            "" => quoted,
            sender => format!("{}: {quoted}", sanitize_line(sender)),
        }
    });
    Some(Entry {
        at: message.sent_at,
        sender: sanitize_line(&message.sender).trim().to_owned(),
        kind,
        // Cleaned here too, whatever was done before: nothing but `\n` ends a line
        // in what is written from this.
        text: sanitize(&text),
        name: name.map(|name| sanitize_line(&name).trim().to_owned()),
        reply,
        edited: message.edited,
        reactions: message
            .reactions
            .iter()
            .map(|reaction| (sanitize_line(&reaction.key), reaction.count))
            .collect(),
    })
}

/// The caption in the text the app shows for a file: what follows its first line
/// (symbol, name, length, size) and, for a voice message, the row of bars under it.
fn caption_of(body: &str) -> String {
    let mut lines = body.lines().skip(1).peekable();
    let bars = |line: &str| !line.is_empty() && line.chars().all(|c| ('▁'..='█').contains(&c));
    if lines.peek().is_some_and(|line| bars(line)) {
        lines.next();
    }
    lines.collect::<Vec<_>>().join("\n")
}

/// The local date and time of `at_ms` in `zone`.
pub fn local<Tz: TimeZone>(zone: &Tz, at_ms: u64) -> Option<DateTime<Tz>> {
    zone.timestamp_millis_opt(i64::try_from(at_ms).ok()?)
        .single()
}

/// One message on one line for a summary command: `[date time] sender: text`. A line
/// break inside a message becomes ` / `.
pub fn summary_line<Tz: TimeZone>(zone: &Tz, entry: &Entry) -> String
where
    Tz::Offset: fmt::Display,
{
    let when = entry
        .at
        .and_then(|at| local(zone, at))
        .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".to_owned());
    let text = entry
        .text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
    let name = entry.name.as_deref().unwrap_or_default();
    let said = match &entry.kind {
        Kind::Text | Kind::Emote | Kind::Event => text,
        Kind::Poll => format!("[poll] {text}"),
        Kind::Deleted => "[message deleted]".to_owned(),
        Kind::Undecryptable => "[message could not be decrypted]".to_owned(),
        Kind::Media(what) if text.is_empty() => format!("[{what}: {name}]"),
        Kind::Media(what) => format!("[{what}: {name}] {text}"),
    };
    let reply = entry
        .reply
        .as_ref()
        .map(|reply| format!(" (replying to {reply})"))
        .unwrap_or_default();
    match entry.kind {
        Kind::Emote => format!("[{when}] * {}{reply} {said}", entry.sender),
        Kind::Event => format!("[{when}] ({said})"),
        Kind::Text | Kind::Deleted | Kind::Undecryptable | Kind::Poll | Kind::Media(_) => {
            format!("[{when}] {}{reply}: {said}", entry.sender)
        }
    }
}

/// The text for a summary command from `lines` (oldest first): the newest of them
/// that fit [`SUMMARY_MESSAGES`] and [`SUMMARY_CHARS`], each ending in a line break.
/// The second value tells how many are in it.
pub fn summary_text(lines: &[String]) -> (String, usize) {
    let mut chars = 0;
    let mut kept = 0;
    for line in lines.iter().rev().take(SUMMARY_MESSAGES) {
        // A single message longer than everything allowed is cut to fit below.
        let length = line.chars().count().min(SUMMARY_CHARS) + 1;
        if chars + length > SUMMARY_CHARS + 1 {
            break;
        }
        chars += length;
        kept += 1;
    }
    let mut text = String::new();
    for line in &lines[lines.len() - kept..] {
        text.extend(line.chars().take(SUMMARY_CHARS));
        text.push('\n');
    }
    (text, kept)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{Attachment, ImageInfo, ImageSource, Reaction, SendState};
    use chrono::Utc;
    use matrix_sdk::ruma::OwnedMxcUri;
    use matrix_sdk::ruma::events::room::MediaSource;

    /// 2026-11-20 14:03 UTC.
    pub const AT: u64 = 1_795_183_380_000;

    pub fn said(sender: &str, body: &str) -> Message {
        Message {
            sender: sender.to_owned(),
            body: body.to_owned(),
            sent_at: Some(AT),
            ..Message::default()
        }
    }

    #[test]
    fn nothing_but_a_newline_ends_a_line_of_a_messages_text() {
        let entry = entry(&said(
            "Ann",
            "one\rtwo\u{2028}three\u{1b}[2J\r\nfour\u{85}five",
        ))
        .unwrap();
        assert_eq!(entry.text, "onetwothree[2J\nfourfive");
        assert_eq!(entry.text.lines().count(), 2);
    }

    fn file(kind: MediaKind, name: &str, body: &str) -> Message {
        let mut message = said("Ann", body);
        message.file = Some(Attachment {
            key: "mxc://example.org/abc".to_owned(),
            name: name.to_owned(),
            size: Some(5),
            kind,
            source: Box::new(MediaSource::Plain(OwnedMxcUri::from(
                "mxc://example.org/abc",
            ))),
            audio: None,
        });
        message
    }

    #[test]
    fn a_text_keeps_its_sender_its_time_and_what_it_says_now() {
        let mut message = said("Ann\u{1b}[31m", "the plan\nfor today");
        message.edited = true;
        message.reactions = vec![Reaction {
            key: "👍".to_owned(),
            count: 2,
            own: false,
        }];
        let entry = entry(&message).unwrap();
        assert_eq!(entry.at, Some(AT));
        assert_eq!(entry.sender, "Ann[31m");
        assert_eq!(entry.kind, Kind::Text);
        assert_eq!(entry.text, "the plan\nfor today");
        assert!(entry.edited);
        assert_eq!(entry.reactions, [("👍".to_owned(), 2)]);
        assert_eq!(
            summary_line(&Utc, &entry),
            "[2026-11-20 14:03] Ann[31m: the plan / for today"
        );
    }

    #[test]
    fn a_reply_names_whom_and_what_it_answers_in_a_few_words() {
        let mut message = said("Ann", "yes");
        message.reply_to = Some(("Bob".to_owned(), "word ".repeat(40)));
        let entry = entry(&message).unwrap();
        let reply = entry.reply.clone().unwrap();
        assert!(reply.starts_with("Bob: word word"), "{reply}");
        assert!(reply.ends_with('…'));
        assert!(crate::text::width(&reply) <= REPLY_QUOTE + 5);
        assert!(summary_line(&Utc, &entry).contains("Ann (replying to Bob: word"));
        // Whom it answers is not always known.
        message.reply_to = Some((String::new(), "an earlier message".to_owned()));
        assert_eq!(
            super::entry(&message).unwrap().reply.as_deref(),
            Some("an earlier message")
        );
    }

    #[test]
    fn deleted_and_undecryptable_messages_are_markers_without_text() {
        let mut message = said("Ann", "Message deleted");
        message.kind = MsgKind::Redacted;
        let deleted = entry(&message).unwrap();
        assert_eq!(
            (deleted.kind.clone(), deleted.text.as_str()),
            (Kind::Deleted, "")
        );
        assert_eq!(
            summary_line(&Utc, &deleted),
            "[2026-11-20 14:03] Ann: [message deleted]"
        );
        message.kind = MsgKind::Undecryptable;
        let unreadable = entry(&message).unwrap();
        assert_eq!(unreadable.kind, Kind::Undecryptable);
        assert_eq!(
            summary_line(&Utc, &unreadable),
            "[2026-11-20 14:03] Ann: [message could not be decrypted]"
        );
    }

    #[test]
    fn media_is_its_kind_and_file_name_with_the_caption_as_text() {
        let voice = entry(&file(
            MediaKind::Voice,
            "voice.ogg",
            "🎤 voice.ogg · 0:05 · 5 B\n▁▃█▃▁\nlisten to this",
        ))
        .unwrap();
        assert_eq!(voice.kind, Kind::Media("Voice message"));
        assert_eq!(voice.name.as_deref(), Some("voice.ogg"));
        assert_eq!(voice.text, "listen to this");
        assert_eq!(
            summary_line(&Utc, &voice),
            "[2026-11-20 14:03] Ann: [Voice message: voice.ogg] listen to this"
        );
        let plain = entry(&file(MediaKind::File, "report.pdf", "📎 report.pdf · 5 B")).unwrap();
        assert_eq!(plain.text, "");
        assert_eq!(
            summary_line(&Utc, &plain),
            "[2026-11-20 14:03] Ann: [File: report.pdf]"
        );

        let mut picture = said("Ann", "🖼  cat.png\nour cat");
        picture.image = Some(ImageInfo {
            key: "mxc://example.org/cat".to_owned(),
            source: ImageSource::Demo,
            name: "cat.png".to_owned(),
            caption: Some("our cat".to_owned()),
            dimensions: None,
            size: None,
            sticker: false,
        });
        let picture = entry(&picture).unwrap();
        assert_eq!(picture.kind, Kind::Media("Picture"));
        assert_eq!(picture.name.as_deref(), Some("cat.png"));
        assert_eq!(picture.text, "our cat");
    }

    #[test]
    fn actions_and_room_events_read_as_such() {
        let mut message = said("Ann", "waves");
        message.kind = MsgKind::Emote;
        assert_eq!(
            summary_line(&Utc, &entry(&message).unwrap()),
            "[2026-11-20 14:03] * Ann waves"
        );
        message.kind = MsgKind::System;
        message.body = "Ann joined".to_owned();
        assert_eq!(
            summary_line(&Utc, &entry(&message).unwrap()),
            "[2026-11-20 14:03] (Ann joined)"
        );
    }

    #[test]
    fn the_new_line_and_messages_still_on_their_way_are_left_out() {
        let mut message = said("Ann", "hello");
        message.kind = MsgKind::ReadMarker;
        assert!(entry(&message).is_none());
        message.kind = MsgKind::Text;
        message.state = SendState::Sending;
        assert!(entry(&message).is_none());
        message.state = SendState::Failed;
        assert!(entry(&message).is_none());
    }

    #[test]
    fn the_text_for_a_summary_keeps_the_newest_that_fit() {
        let lines: Vec<String> = (0..SUMMARY_MESSAGES + 20)
            .map(|n| format!("line {n}"))
            .collect();
        let (text, kept) = summary_text(&lines);
        assert_eq!(kept, SUMMARY_MESSAGES);
        assert!(text.starts_with("line 20\n"), "{}", &text[..20]);
        assert!(text.ends_with(&format!("line {}\n", SUMMARY_MESSAGES + 19)));

        // Too much text: the oldest go first.
        let long: Vec<String> = (0..10)
            .map(|n| format!("{n}{}", "x".repeat(4999)))
            .collect();
        let (text, kept) = summary_text(&long);
        assert_eq!(kept, 4);
        assert!(text.chars().count() <= SUMMARY_CHARS + 1);
        assert!(text.starts_with('6'));

        // One message longer than everything allowed is cut, not dropped.
        let (text, kept) = summary_text(&["y".repeat(SUMMARY_CHARS * 2)]);
        assert_eq!(kept, 1);
        assert_eq!(text.chars().count(), SUMMARY_CHARS + 1);

        assert_eq!(summary_text(&[]), (String::new(), 0));
    }

    #[test]
    fn a_span_of_days_starts_that_many_days_ago() {
        assert_eq!(Span::Days(7).since(AT), Some(AT - 7 * 86_400_000));
        assert_eq!(Span::Days(7).since(5), Some(0));
        assert_eq!(Span::Loaded.since(AT), None);
        assert_eq!(Span::Everything.since(AT), None);
        assert_eq!(Span::Days(30).label(), "The last 30 days");
    }

    #[test]
    fn the_debug_output_of_an_entry_says_nothing_of_its_text() {
        let mut message = said("Ann Secret", "the secret plan");
        message.reply_to = Some(("Bob Secret".to_owned(), "hidden words".to_owned()));
        let shown = format!("{:?}", entry(&message).unwrap());
        for private in ["Ann", "Bob", "secret", "hidden"] {
            assert!(!shown.contains(private), "{shown}");
        }
    }
}
