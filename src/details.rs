//! The message details popup: who sent a message and when to the second, its event
//! ID, whether it was encrypted and how far it can be trusted, how far ours got, who
//! has read up to it, and its source.

use std::fmt::Display;

use chrono::{DateTime, Local, TimeZone};

use crate::matrix::{EventSource, SOURCE_MAX, SourceKind};
use crate::model::{ItemRef, Message, MsgKind, SendState, Shield};
use crate::text::sanitize_line;

/// Which page of the popup is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailsView {
    Facts,
    Source,
}

/// The source while it is asked for, once it is there, or why there is none.
#[derive(Clone)]
pub enum SourceState {
    Loading,
    Ready(Box<EventSource>),
    Missing(String),
}

/// How a line is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Muted,
    Warning,
}

/// One fact: a label and its value.
#[derive(Clone, PartialEq, Eq)]
pub struct Fact {
    pub label: &'static str,
    pub value: String,
    pub tone: Tone,
}

#[derive(Clone)]
pub struct DetailsPopup {
    /// Numbers the request whose answer belongs to this popup.
    pub request: u64,
    sender: String,
    sender_id: Option<String>,
    sent: String,
    edited: bool,
    /// When the latest edit was sent, once the source has said.
    edited_at: Option<String>,
    pub event_id: Option<String>,
    encryption: (String, bool),
    /// How far a message of ours got; `None` for other people's.
    delivery: Option<String>,
    readers: Vec<String>,
    more_readers: usize,
    pub source: SourceState,
    pub view: DetailsView,
    /// First line shown, and how many fit (as of the last draw).
    pub scroll: usize,
    pub page: usize,
    /// The source's lines wrapped to the width they were last drawn at.
    pub wrapped: Option<(usize, Vec<(String, Tone)>)>,
}

/// Hand-written: who sent what, and the source, stay out of logs.
impl std::fmt::Debug for DetailsPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DetailsPopup")
            .field("request", &self.request)
            .field("view", &self.view)
            .field("loading", &matches!(self.source, SourceState::Loading))
            .finish_non_exhaustive()
    }
}

impl DetailsPopup {
    /// The details of `message`, as far as the timeline knows them. `room_encrypted`
    /// says what becomes of a message of ours that is not sent yet; `show_readers`
    /// is the `read_markers` setting.
    pub fn new(message: &Message, room_encrypted: bool, show_readers: bool, request: u64) -> Self {
        let event_id = match &message.item {
            Some(ItemRef::Event(id)) => Some(id.clone()),
            Some(ItemRef::Transaction(_)) => None,
            // The demo's messages have made-up ids at most.
            None => None,
        };
        let delivery = message.own.then(|| match message.state {
            SendState::Sent => "Sent".to_owned(),
            SendState::Sending => message.sending_label(),
            SendState::Failed => message.failed_label(),
        });
        let (readers, more_readers) = if show_readers {
            let names: Vec<String> = message
                .read_by
                .readers
                .iter()
                .map(|reader| {
                    let name = sanitize_line(&reader.name);
                    match sanitize_line(&reader.time) {
                        time if time.is_empty() => name,
                        time => format!("{name} · {time}"),
                    }
                })
                .collect();
            let more = (message.read_by.total as usize).saturating_sub(names.len());
            (names, more)
        } else {
            (Vec::new(), 0)
        };
        Self {
            request,
            sender: sanitize_line(&message.sender),
            sender_id: message.sender_id.as_deref().map(sanitize_line),
            sent: message.sent_at.and_then(local).map_or_else(
                || {
                    let rough = format!("{} {}", message.day, message.time);
                    match sanitize_line(rough.trim()) {
                        rough if rough.is_empty() => "not known".to_owned(),
                        rough => rough,
                    }
                },
                |at| exact(&at),
            ),
            edited: message.edited,
            edited_at: None,
            event_id: event_id.map(|id| sanitize_line(&id)),
            encryption: encryption(message, room_encrypted),
            delivery,
            readers,
            more_readers,
            source: SourceState::Loading,
            view: DetailsView::Facts,
            scroll: 0,
            page: 10,
            wrapped: None,
        }
    }

    /// Takes the worker's answer.
    pub fn set_source(&mut self, result: Result<Box<EventSource>, String>) {
        self.source = match result {
            Ok(source) => {
                self.edited_at = source.edited_at.and_then(local).map(|at| exact(&at));
                SourceState::Ready(source)
            }
            Err(reason) => SourceState::Missing(sanitize_line(&reason)),
        };
        self.wrapped = None;
    }

    /// The facts, top to bottom. A label is given once, for the first line of its value.
    pub fn facts(&self) -> Vec<Fact> {
        let fact = |label, value: String, tone| Fact { label, value, tone };
        let mut out = vec![fact(
            "From",
            if self.sender.is_empty() {
                "not known".to_owned()
            } else {
                self.sender.clone()
            },
            Tone::Normal,
        )];
        if let Some(id) = &self.sender_id {
            out.push(fact("", id.clone(), Tone::Muted));
        }
        out.push(fact("Sent", self.sent.clone(), Tone::Normal));
        if self.edited {
            let when = match (&self.edited_at, &self.source) {
                (Some(at), _) => at.clone(),
                (None, SourceState::Loading) => "yes (asking when…)".to_owned(),
                (None, _) => "yes (when is not known)".to_owned(),
            };
            out.push(fact("Edited", when, Tone::Normal));
        }
        out.push(fact(
            "Event ID",
            self.event_id
                .clone()
                .unwrap_or_else(|| "none yet (not on the server)".to_owned()),
            Tone::Normal,
        ));
        let (security, warning) = &self.encryption;
        out.push(fact(
            "Security",
            security.clone(),
            if *warning {
                Tone::Warning
            } else {
                Tone::Normal
            },
        ));
        if let Some(delivery) = &self.delivery {
            out.push(fact("Delivery", delivery.clone(), Tone::Normal));
        }
        for (n, reader) in self.readers.iter().enumerate() {
            let label = if n == 0 { "Read by" } else { "" };
            out.push(fact(label, reader.clone(), Tone::Normal));
        }
        if self.more_readers > 0 {
            let label = if self.readers.is_empty() {
                "Read by"
            } else {
                ""
            };
            out.push(fact(
                label,
                format!("and {} more", self.more_readers),
                Tone::Muted,
            ));
        }
        out
    }

    /// The source page, line by line before wrapping: what is shown, then the JSON.
    pub fn source_lines(&self) -> Vec<(String, Tone)> {
        let source = match &self.source {
            SourceState::Loading => return vec![("Loading the source…".to_owned(), Tone::Muted)],
            SourceState::Missing(reason) => return vec![(reason.clone(), Tone::Muted)],
            SourceState::Ready(source) => source,
        };
        let what = match source.kind {
            SourceKind::Plain => "The event as the server sent it:",
            SourceKind::Decrypted => {
                "The event as this device decrypted it (the server holds it encrypted):"
            }
            SourceKind::Encrypted => {
                "The encrypted event, as the server holds it: this device could not decrypt it."
            }
        };
        let mut out = vec![(what.to_owned(), Tone::Muted)];
        out.extend(
            source
                .json
                .lines()
                .map(|line| (line.to_owned(), Tone::Normal)),
        );
        if let Some(edit) = &source.edit_json {
            out.push((String::new(), Tone::Normal));
            out.push(("Its latest edit:".to_owned(), Tone::Muted));
            out.extend(edit.lines().map(|line| (line.to_owned(), Tone::Normal)));
        }
        if source.cut {
            out.push((
                format!("… cut here: longer than {} KB", SOURCE_MAX / 1024),
                Tone::Warning,
            ));
        }
        out
    }

    /// What copying on the source page puts on the clipboard: the JSON, and the latest
    /// edit's after a blank line. `Err` says why there is nothing.
    pub fn source_json(&self) -> Result<String, String> {
        match &self.source {
            SourceState::Loading => Err("The source has not arrived yet".to_owned()),
            SourceState::Missing(reason) => Err(reason.clone()),
            SourceState::Ready(source) => Ok(match &source.edit_json {
                Some(edit) => format!("{}\n\n{edit}", source.json),
                None => source.json.clone(),
            }),
        }
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add_signed(lines);
    }

    /// Switches between the facts and the source, from the top.
    pub fn flip(&mut self) {
        self.view = match self.view {
            DetailsView::Facts => DetailsView::Source,
            DetailsView::Source => DetailsView::Facts,
        };
        self.scroll = 0;
    }
}

fn local(millis: u64) -> Option<DateTime<Local>> {
    Local
        .timestamp_millis_opt(i64::try_from(millis).ok()?)
        .single()
}

/// A moment to the second, with the date and the offset from UTC.
pub fn exact<Tz: TimeZone>(at: &DateTime<Tz>) -> String
where
    Tz::Offset: Display,
{
    at.format("%a %-d %b %Y, %H:%M:%S (UTC%:z)").to_string()
}

/// Whether the message was encrypted, in words that say how far it can be trusted
/// (the wording of the timeline's warnings), and whether that is a warning.
fn encryption(message: &Message, room_encrypted: bool) -> (String, bool) {
    if message.kind == MsgKind::Undecryptable {
        let why = message
            .utd
            .map_or("Unable to decrypt", crate::model::UtdWhy::text);
        return (format!("Encrypted · {why}"), true);
    }
    match message.shield {
        Some(Shield::NotEncrypted) => ("Not encrypted, in an encrypted room".to_owned(), true),
        Some(shield) if message.encrypted => {
            (format!("Encrypted · {}", shield.label()), shield.serious())
        }
        _ if message.encrypted => ("Encrypted · no doubts about the sender".to_owned(), false),
        _ if message.own && message.state != SendState::Sent && room_encrypted => {
            ("Encrypted when it is sent".to_owned(), false)
        }
        _ => ("Not encrypted".to_owned(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReadBy, Reader, UtdWhy};
    use chrono::FixedOffset;

    fn message() -> Message {
        Message {
            id: "x".into(),
            item: Some(ItemRef::Event("$ev:example.org".into())),
            sender: "Alice\u{1b}[31m".into(),
            sender_id: Some("@alice:example.org".into()),
            day: "Today".into(),
            time: "14:05".into(),
            body: "secret words".into(),
            ..Message::default()
        }
    }

    fn value(popup: &DetailsPopup, label: &str) -> String {
        popup
            .facts()
            .into_iter()
            .find(|fact| fact.label == label)
            .map(|fact| fact.value)
            .unwrap_or_else(|| panic!("no {label}"))
    }

    fn source(kind: SourceKind, edit: bool) -> Box<EventSource> {
        Box::new(EventSource {
            kind,
            json: "{\n  \"type\": \"m.room.message\"\n}".into(),
            edit_json: edit.then(|| "{\n  \"type\": \"edit\"\n}".into()),
            edited_at: edit.then_some(1_700_000_000_000),
            cut: false,
        })
    }

    #[test]
    fn times_are_given_to_the_second_with_the_offset() {
        let at = FixedOffset::east_opt(3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 14, 14, 5, 33)
            .unwrap();
        assert_eq!(exact(&at), "Sat 14 Nov 2026, 14:05:33 (UTC+01:00)");
    }

    #[test]
    fn facts_name_the_sender_safely_and_fall_back_to_the_rough_time() {
        let popup = DetailsPopup::new(&message(), false, true, 1);
        assert_eq!(value(&popup, "From"), "Alice[31m");
        assert!(
            popup
                .facts()
                .iter()
                .any(|fact| fact.label.is_empty() && fact.value == "@alice:example.org")
        );
        assert_eq!(value(&popup, "Sent"), "Today 14:05");
        assert_eq!(value(&popup, "Event ID"), "$ev:example.org");
        assert_eq!(value(&popup, "Security"), "Not encrypted");
        // Other people's messages have no delivery to speak of.
        assert!(popup.facts().iter().all(|fact| fact.label != "Delivery"));

        let mut with_time = message();
        with_time.sent_at = Some(1_700_000_000_000);
        let popup = DetailsPopup::new(&with_time, false, true, 1);
        assert_eq!(
            value(&popup, "Sent"),
            exact(&local(1_700_000_000_000).unwrap())
        );
    }

    #[test]
    fn security_uses_the_timeline_wording() {
        let mut encrypted = message();
        encrypted.encrypted = true;
        let popup = DetailsPopup::new(&encrypted, true, true, 1);
        assert_eq!(
            value(&popup, "Security"),
            "Encrypted · no doubts about the sender"
        );
        encrypted.shield = Some(Shield::UnverifiedDevice);
        let popup = DetailsPopup::new(&encrypted, true, true, 1);
        assert_eq!(
            value(&popup, "Security"),
            format!("Encrypted · {}", Shield::UnverifiedDevice.label())
        );
        assert!(popup.facts().iter().any(|fact| fact.tone == Tone::Warning));

        let mut locked = message();
        locked.kind = MsgKind::Undecryptable;
        locked.utd = Some(UtdWhy::SentBeforeJoin);
        let popup = DetailsPopup::new(&locked, true, true, 1);
        assert_eq!(
            value(&popup, "Security"),
            format!("Encrypted · {}", UtdWhy::SentBeforeJoin.text())
        );

        let mut clear = message();
        clear.shield = Some(Shield::NotEncrypted);
        let popup = DetailsPopup::new(&clear, true, true, 1);
        assert_eq!(
            value(&popup, "Security"),
            "Not encrypted, in an encrypted room"
        );
    }

    #[test]
    fn our_own_messages_say_how_far_they_got() {
        let mut own = message();
        own.own = true;
        own.state = SendState::Failed;
        own.send_error = Some("too large".into());
        own.item = Some(ItemRef::Transaction("t1".into()));
        let popup = DetailsPopup::new(&own, true, true, 1);
        assert_eq!(value(&popup, "Delivery"), "✕ not sent: too large");
        assert_eq!(value(&popup, "Event ID"), "none yet (not on the server)");
        assert_eq!(value(&popup, "Security"), "Encrypted when it is sent");
    }

    #[test]
    fn readers_are_listed_when_read_markers_are_wanted() {
        let mut read = message();
        read.read_by = ReadBy {
            readers: vec![
                Reader {
                    name: "bob".into(),
                    time: "14:06".into(),
                },
                Reader {
                    name: "carol\u{7}".into(),
                    time: String::new(),
                },
            ],
            total: 5,
        };
        let popup = DetailsPopup::new(&read, false, true, 1);
        let lines: Vec<(&str, String)> = popup
            .facts()
            .into_iter()
            .skip_while(|fact| fact.label != "Read by")
            .map(|fact| (fact.label, fact.value))
            .collect();
        assert_eq!(
            lines,
            [
                ("Read by", "bob · 14:06".to_owned()),
                ("", "carol".to_owned()),
                ("", "and 3 more".to_owned()),
            ]
        );
        let quiet = DetailsPopup::new(&read, false, false, 1);
        assert!(quiet.facts().iter().all(|fact| fact.label != "Read by"));
    }

    #[test]
    fn the_source_says_which_json_it_is_and_brings_the_edit_time() {
        let mut edited = message();
        edited.edited = true;
        let mut popup = DetailsPopup::new(&edited, true, true, 1);
        assert_eq!(value(&popup, "Edited"), "yes (asking when…)");
        assert_eq!(popup.source_lines()[0].0, "Loading the source…");
        assert!(popup.source_json().is_err());

        popup.set_source(Ok(source(SourceKind::Decrypted, true)));
        assert_eq!(
            value(&popup, "Edited"),
            exact(&local(1_700_000_000_000).unwrap())
        );
        let lines = popup.source_lines();
        assert!(lines[0].0.contains("decrypted"));
        assert!(lines.iter().any(|(line, _)| line == "Its latest edit:"));
        assert_eq!(
            popup.source_json().unwrap(),
            "{\n  \"type\": \"m.room.message\"\n}\n\n{\n  \"type\": \"edit\"\n}"
        );

        popup.set_source(Ok(source(SourceKind::Encrypted, false)));
        assert!(popup.source_lines()[0].0.contains("could not decrypt"));
        assert_eq!(value(&popup, "Edited"), "yes (when is not known)");

        popup.set_source(Err("This message is no longer loaded\u{1b}".into()));
        assert_eq!(
            popup.source_lines(),
            [("This message is no longer loaded".to_owned(), Tone::Muted)]
        );
    }

    #[test]
    fn debug_output_shows_nothing_of_the_message() {
        let mut popup = DetailsPopup::new(&message(), false, true, 1);
        popup.set_source(Ok(source(SourceKind::Plain, true)));
        let debug = format!("{popup:?}");
        for secret in ["Alice", "alice", "$ev", "secret", "m.room.message"] {
            assert!(!debug.contains(secret), "{secret} in {debug}");
        }
    }
}
