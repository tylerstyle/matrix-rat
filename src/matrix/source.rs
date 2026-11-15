//! A message's source for the message details popup: its JSON as this device holds
//! it, made safe to put on the screen and bounded in size.

use matrix_sdk::ruma::{EventId, MilliSecondsSinceUnixEpoch};
use matrix_sdk_ui::timeline::Timeline;

use crate::model::ItemRef;

/// One JSON document of the source may be this long; the rest is cut. Events are at
/// most 64 KB, so only a misbehaving server comes near it. An event and its latest
/// edit stay within 256 KB together.
pub const SOURCE_MAX: usize = 128 * 1024;

const NOT_SENT: &str = "This message has not reached the server yet, so it has no source";

/// Which JSON the source shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Not encrypted: the event as the server sent it.
    Plain,
    /// Encrypted, shown as this device decrypted it.
    Decrypted,
    /// Encrypted and not decrypted here: the encrypted event.
    Encrypted,
}

/// What [`super::MatrixCmd::FetchSource`] found.
#[derive(Clone, PartialEq, Eq)]
pub struct EventSource {
    pub kind: SourceKind,
    /// Pretty-printed, safe for the terminal, at most [`SOURCE_MAX`] bytes.
    pub json: String,
    /// The latest edit's JSON in the same form, when the message was edited.
    pub edit_json: Option<String>,
    /// When the latest edit was sent, in milliseconds since 1970.
    pub edited_at: Option<u64>,
    /// Something was longer than [`SOURCE_MAX`] and is cut.
    pub cut: bool,
}

/// Lengths only: the JSON is message content.
impl std::fmt::Debug for EventSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSource")
            .field("kind", &self.kind)
            .field("json_bytes", &self.json.len())
            .field("edit_bytes", &self.edit_json.as_ref().map(String::len))
            .field("edited_at", &self.edited_at.is_some())
            .field("cut", &self.cut)
            .finish()
    }
}

/// The source of `item` in `timeline`, or why there is none. Only what the timeline
/// holds already is used: nothing is asked of the server.
pub(super) async fn fetch(timeline: &Timeline, item: &ItemRef) -> Result<EventSource, String> {
    let ItemRef::Event(event_id) = item else {
        return Err(NOT_SENT.to_owned());
    };
    let event_id = EventId::parse(event_id).map_err(|_| "That is not an event ID".to_owned())?;
    let found = timeline
        .item_by_event_id(&event_id)
        .await
        .ok_or_else(|| "This message is no longer loaded".to_owned())?;
    let original = found.original_json().ok_or_else(|| NOT_SENT.to_owned())?;
    // The timeline keeps decrypted events as they were decrypted; an event that could
    // not be decrypted is kept as it came.
    let kind = if found.content().is_unable_to_decrypt() {
        SourceKind::Encrypted
    } else if found.encryption_info().is_some() {
        SourceKind::Decrypted
    } else {
        SourceKind::Plain
    };
    let (json, cut) = pretty(original.json().get(), SOURCE_MAX);
    let edit = found.latest_edit_json();
    let edited_at = edit
        .and_then(|raw| {
            raw.get_field::<MilliSecondsSinceUnixEpoch>("origin_server_ts")
                .ok()
                .flatten()
        })
        .map(|at| u64::from(at.get()));
    let (edit_json, edit_cut) = match edit {
        Some(raw) => {
            let (json, cut) = pretty(raw.json().get(), SOURCE_MAX);
            (Some(json), cut)
        }
        None => (None, false),
    };
    Ok(EventSource {
        kind,
        json,
        edit_json,
        edited_at,
        cut: cut || edit_cut,
    })
}

/// `raw` pretty-printed, with every character that could act on the terminal written
/// as a JSON escape (inside JSON they can only be in strings, so it stays valid JSON),
/// cut at `max` bytes. What does not read as JSON (or is too long or too deeply nested
/// to read) is shown as it came, made safe the same way. `true` if it was cut.
pub(super) fn pretty(raw: &str, max: usize) -> (String, bool) {
    use std::fmt::Write as _;
    let pretty = if raw.len() <= max {
        serde_json::from_str::<serde_json::Value>(raw)
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
    } else {
        None
    };
    let text = pretty.as_deref().unwrap_or(raw);
    let mut safe = String::with_capacity(text.len().min(max + 16));
    let mut cut = false;
    for ch in text.chars() {
        let len = safe.len();
        if crate::text::unsafe_char(ch) && ch != '\n' {
            let _ = write!(safe, "\\u{:04x}", u32::from(ch));
        } else {
            safe.push(ch);
        }
        if safe.len() > max {
            safe.truncate(len);
            cut = true;
            break;
        }
    }
    // Nothing unsafe is left; this only guards the rule that all shown text passes here.
    (crate::text::sanitize(&safe), cut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_is_pretty_printed() {
        let (text, cut) = pretty(r#"{"type":"m.room.message","content":{"body":"hi"}}"#, 1000);
        assert!(!cut);
        assert!(
            text.contains("\n  \"content\": {\n    \"body\": \"hi\"\n  }"),
            "{text}"
        );
        assert!(text.starts_with('{') && text.ends_with('}'));
    }

    #[test]
    fn what_could_act_on_the_terminal_becomes_an_escape_and_stays_json() {
        // DEL, a C1 control and a direction override are not escaped by JSON itself.
        let raw = "{\"body\":\"a\u{7f}b\u{9b}c\u{202e}d\\u001b[31m\"}";
        let (text, _) = pretty(raw, 1000);
        for bad in ['\u{7f}', '\u{9b}', '\u{202e}', '\u{1b}'] {
            assert!(!text.contains(bad), "{text:?}");
        }
        let escaped = concat!(r"a\u007fb\u009bc\u", "202e", r"d\u001b[31m");
        assert!(text.contains(escaped), "{text}");
        let back: serde_json::Value = serde_json::from_str(&text).expect("still JSON");
        assert_eq!(back["body"], "a\u{7f}b\u{9b}c\u{202e}d\u{1b}[31m");
    }

    #[test]
    fn what_is_not_json_is_shown_as_it_came_but_safe() {
        let (text, cut) = pretty("not json \u{1b}]52;c;x\u{7}", 1000);
        assert!(!cut);
        assert_eq!(text, r"not json \u001b]52;c;x\u0007");
    }

    #[test]
    fn a_long_source_is_cut_at_the_bound() {
        let raw = format!("{{\"body\":\"{}\"}}", "é".repeat(5000));
        let (text, cut) = pretty(&raw, 1001);
        assert!(cut);
        assert!(text.len() <= 1001);
        // Too long to read: shown as it came, cut on a character boundary.
        let (text, cut) = pretty(&"x".repeat(5000), 100);
        assert!(cut && text.len() == 100);
        let (_, cut) = pretty("{}", 2);
        assert!(!cut);
    }

    #[test]
    fn debug_output_shows_no_content() {
        let source = EventSource {
            kind: SourceKind::Decrypted,
            json: "{\"body\":\"secret words\"}".into(),
            edit_json: Some("{\"body\":\"other secret\"}".into()),
            edited_at: Some(1),
            cut: false,
        };
        let debug = format!("{source:?}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
