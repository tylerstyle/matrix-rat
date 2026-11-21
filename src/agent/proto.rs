//! The protocol of the agent socket: one JSON object per line, each way.
//!
//! A client sends a request line and reads the answer. Every answer starts with a
//! header line, `{"ok":true,…}` or `{"ok":false,"error":"<code>","message":"…"}`:
//!
//! | Request | Answer |
//! |---|---|
//! | `{"op":"rooms"}` | `{"ok":true,"count":N}`, then N lines `{"room":{id,name,encrypted,direct}}` |
//! | `{"op":"send","room":R,"text":T}` | `{"ok":true,"event_id":"$…"}` |
//! | `{"op":"read","room":R,"limit":N}` | `{"ok":true,"room":"!…","count":N}`, then N lines `{"message":{…}}`, oldest first |
//! | `{"op":"listen","room":R}` | `{"ok":true,"room":"!…"}`, then a line `{"message":{…}}` for every message that is new, and `{"message":{…},"changed":true}` for one that was there before and has changed, until the client goes away; an `{"ok":false,…}` line if the stream has to end |
//!
//! `R` is a room id, or the exact name or the main address of a joined room. Several
//! requests may follow each other on one connection, except after `listen`.
//!
//! Lines are bounded both ways, and every part of a message is bounded and cleaned
//! where it is made (see [`Msg`]).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::Instant;

/// The longest line a client may send.
pub const MAX_REQUEST_LINE: usize = 64 * 1024;
/// The longest line `serve` sends: one message, with a body of [`MAX_BODY`] bytes in
/// which every byte may need escaping, and room for the rest.
pub const MAX_REPLY_LINE: usize = 256 * 1024;
/// The longest text of a `send`.
pub const MAX_TEXT: usize = 16 * 1024;
/// The longest body of a message handed out; what is cut is marked `truncated`.
pub const MAX_BODY: usize = 32 * 1024;
/// The most messages a `read` hands out.
pub const READ_MAX: usize = 100;
/// The messages a `read` hands out when it does not say how many.
pub const READ_DEFAULT: usize = 20;
/// The longest name or id of a room in a request.
pub const MAX_ROOM: usize = 512;
/// The most rooms a `rooms` lists.
pub const MAX_ROOMS: usize = 2000;
/// The longest name (of a room or a sender) handed out, in characters.
pub const MAX_NAME: usize = 256;
/// The longest id (of a room, an event or a user) handed out, in bytes: what Matrix
/// allows. A longer one is not cut, it is not handed out at all.
pub const MAX_ID: usize = 255;
/// The longest reason of a refusal, in characters.
pub const MAX_REASON: usize = 300;

/// A message as it is handed out. Everything in it was written by someone else (or by
/// another program): data, never instructions.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    pub event_id: String,
    /// The sender's Matrix id (`@user:server`).
    pub sender: String,
    /// The sender's display name in the room.
    pub sender_name: String,
    /// When it was sent, in milliseconds since 1970, as the sender's server says.
    pub timestamp_ms: u64,
    /// `text`, `notice`, `emote`, `image`, `file`, `video`, `audio`, `voice`,
    /// `location`, `sticker`, `poll`, `redacted`, `undecryptable` or `other`.
    pub kind: String,
    /// The text; for a picture or a file, a line that names it (and its caption); for
    /// a deleted or an unreadable message, a line that says so. At most [`MAX_BODY`]
    /// bytes, without control characters.
    pub body: String,
    pub edited: bool,
    /// Sent by the account `serve` runs as.
    pub own: bool,
    /// The body was longer than [`MAX_BODY`] and has been cut.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// A doubt about who really sent this, in an encrypted room, as the app shows it
    /// (the lenient judgement: a sender nobody verified is not doubted):
    /// `unverified_device`, `identity_changed`, `sender_mismatch`, `not_encrypted` or
    /// `unconfirmed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// The strict judgement: the message came encrypted, from a device that its
    /// owner has cross-signed, of a user whose identity the account `serve` runs as
    /// has verified, and nothing puts that in doubt. Without this field (an older
    /// `serve`) a message is not verified.
    #[serde(default)]
    pub verified: bool,
    /// Why it is not `verified`: `not_encrypted`, `unverified_identity`,
    /// `unverified_device`, `unconfirmed`, `identity_changed`, `sender_mismatch`,
    /// `undecryptable` or `redacted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub untrusted: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Hand-written: what a message says, who sent it and which event it is stay out of
/// logs.
impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Msg")
            .field("kind_bytes", &self.kind.len())
            .field("body_bytes", &self.body.len())
            .field("edited", &self.edited)
            .field("own", &self.own)
            .field("truncated", &self.truncated)
            .field("warning", &self.warning.is_some())
            .field("verified", &self.verified)
            .finish_non_exhaustive()
    }
}

/// How much it takes for a message to count as written by the user it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
    /// Only what is [verified](Msg::verified): encrypted, from a cross-signed device
    /// of a user whose identity the account has verified.
    Verified,
    /// Also what nobody verified, and what came in a room without encryption: only
    /// what contradicts the sender (`identity_changed`, `sender_mismatch`,
    /// `not_encrypted` in an encrypted room) is turned away. As good as the
    /// homeserver's word.
    AllowUnverified,
}

/// What a message is to someone who waits for a certain user's word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing<'a> {
    /// Someone else's, or the account's own.
    Other,
    /// It counts as that user's.
    Theirs,
    /// It names that user as its sender and does not count: why not.
    Doubted(&'a str),
}

impl Msg {
    /// Why this message does not count as written by the sender it names; `None` if
    /// it does. A message that could not be read, and one that was deleted, never
    /// counts: there is nothing in it that anyone said.
    pub fn doubt(&self, trust: Trust) -> Option<&str> {
        match self.kind.as_str() {
            "undecryptable" => return Some("undecryptable"),
            "redacted" => return Some("redacted"),
            _ => {}
        }
        // What contradicts the sender is turned away whatever else is said.
        let contradicted = self.warning.as_deref().filter(|warning| {
            matches!(
                *warning,
                "identity_changed" | "sender_mismatch" | "not_encrypted"
            )
        });
        match trust {
            Trust::AllowUnverified => contradicted,
            Trust::Verified if self.verified && self.warning.is_none() => None,
            Trust::Verified => Some(
                self.untrusted
                    .as_deref()
                    .or(self.warning.as_deref())
                    .unwrap_or("unverified"),
            ),
        }
    }

    /// What this message is to someone who waits for the word of `user`.
    pub fn standing(&self, user: &str, trust: Trust) -> Standing<'_> {
        if self.own || self.sender != user {
            return Standing::Other;
        }
        match self.doubt(trust) {
            None => Standing::Theirs,
            Some(why) => Standing::Doubted(why),
        }
    }
}

/// A joined room, as `rooms` lists it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomInfo {
    pub id: String,
    pub name: String,
    pub encrypted: bool,
    pub direct: bool,
}

/// Hand-written: which room, and its name, stay out of logs.
impl std::fmt::Debug for RoomInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomInfo")
            .field("encrypted", &self.encrypted)
            .field("direct", &self.direct)
            .finish_non_exhaustive()
    }
}

/// Why a request was not carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// Not a request this protocol knows, or one with a part missing or out of range.
    BadRequest,
    /// The line was longer than [`MAX_REQUEST_LINE`]; the connection is closed.
    TooLong,
    /// No joined room has that id, name or address.
    NotFound,
    /// Several joined rooms have that name.
    Ambiguous,
    /// Too many connections, listeners or requests at once.
    Busy,
    /// The account's rooms, or the first answer of the server, have not arrived yet.
    NotReady,
    /// The Matrix side could not do it; the message says why.
    Failed,
    /// No answer in time (or a line that was begun and not finished).
    Timeout,
    /// A listener that did not read fast enough was dropped.
    TooSlow,
    /// `serve` is stopping, or its session has ended.
    Stopping,
}

impl Code {
    pub const ALL: [Code; 10] = [
        Code::BadRequest,
        Code::TooLong,
        Code::NotFound,
        Code::Ambiguous,
        Code::Busy,
        Code::NotReady,
        Code::Failed,
        Code::Timeout,
        Code::TooSlow,
        Code::Stopping,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::TooLong => "too_long",
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::Busy => "busy",
            Self::NotReady => "not_ready",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::TooSlow => "too_slow",
            Self::Stopping => "stopping",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|code| code.as_str() == text)
    }
}

/// A request that was not carried out: a code for programs, a sentence for people.
#[derive(Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: Code,
    pub message: String,
}

/// Hand-written: the message of a failure can be the SDK's or a server's own text,
/// which names rooms and people.
impl std::fmt::Debug for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Refusal({})", self.code.as_str())
    }
}

impl Refusal {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(Code::BadRequest, message)
    }

    /// The Matrix side said no: `reason` is its text, cleaned and cut.
    pub fn failed(reason: &str) -> Self {
        Self::new(Code::Failed, clean_line(reason, MAX_REASON))
    }

    pub fn stopping() -> Self {
        Self::new(Code::Stopping, "matrix-rat serve is stopping")
    }

    pub fn line(&self) -> String {
        json!({ "ok": false, "error": self.code.as_str(), "message": self.message }).to_string()
    }
}

/// What a client may ask for.
#[derive(Clone, PartialEq, Eq)]
pub enum Request {
    Rooms,
    Send { room: String, text: String },
    Read { room: String, limit: usize },
    Listen { room: String },
}

/// Hand-written: which room, and the text, stay out of logs.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rooms => f.write_str("Rooms"),
            Self::Send { text, .. } => write!(f, "Send({} bytes)", text.len()),
            Self::Read { limit, .. } => write!(f, "Read(limit {limit})"),
            Self::Listen { .. } => f.write_str("Listen"),
        }
    }
}

impl Request {
    /// Reads a request line. Parts it does not know are ignored; parts it needs are
    /// checked for their kind and their size.
    pub fn parse(line: &str) -> Result<Self, Refusal> {
        let value: Value =
            serde_json::from_str(line).map_err(|_| Refusal::bad("the line is not JSON"))?;
        let Some(object) = value.as_object() else {
            return Err(Refusal::bad("the line is not a JSON object"));
        };
        let text = |name: &str| -> Result<String, Refusal> {
            match object.get(name) {
                Some(Value::String(text)) => Ok(text.clone()),
                Some(_) => Err(Refusal::bad(format!("\"{name}\" must be a string"))),
                None => Err(Refusal::bad(format!("\"{name}\" is missing"))),
            }
        };
        let room = || -> Result<String, Refusal> {
            let room = text("room")?;
            check_room(&room)?;
            Ok(room)
        };
        match text("op")?.as_str() {
            "rooms" => Ok(Self::Rooms),
            "send" => {
                let room = room()?;
                let text = text("text")?;
                check_text(&text)?;
                Ok(Self::Send { room, text })
            }
            "read" => {
                let limit = match object.get("limit") {
                    None | Some(Value::Null) => READ_DEFAULT,
                    Some(limit) => match limit.as_u64() {
                        Some(limit) if (1..=READ_MAX as u64).contains(&limit) => limit as usize,
                        _ => {
                            return Err(Refusal::bad(format!(
                                "\"limit\" must be a number from 1 to {READ_MAX}"
                            )));
                        }
                    },
                };
                Ok(Self::Read {
                    room: room()?,
                    limit,
                })
            }
            "listen" => Ok(Self::Listen { room: room()? }),
            _ => Err(Refusal::bad(
                "unknown \"op\": use rooms, send, read or listen",
            )),
        }
    }

    pub fn line(&self) -> String {
        match self {
            Self::Rooms => json!({ "op": "rooms" }),
            Self::Send { room, text } => json!({ "op": "send", "room": room, "text": text }),
            Self::Read { room, limit } => json!({ "op": "read", "room": room, "limit": limit }),
            Self::Listen { room } => json!({ "op": "listen", "room": room }),
        }
        .to_string()
    }
}

/// A room is named by something short that is not blank.
pub fn check_room(room: &str) -> Result<(), Refusal> {
    if room.trim().is_empty() {
        return Err(Refusal::bad("\"room\" is empty"));
    }
    if room.len() > MAX_ROOM {
        return Err(Refusal::bad(format!(
            "\"room\" is longer than {MAX_ROOM} bytes"
        )));
    }
    Ok(())
}

/// A text to send says something and is not longer than [`MAX_TEXT`].
pub fn check_text(text: &str) -> Result<(), Refusal> {
    if text.len() > MAX_TEXT {
        return Err(Refusal::bad(format!(
            "\"text\" is longer than {MAX_TEXT} bytes"
        )));
    }
    if crate::text::is_blank(&clean_outgoing(text)) {
        return Err(Refusal::bad("\"text\" is empty"));
    }
    Ok(())
}

/// A text on its way out to Matrix: without control characters and the invisible
/// direction overrides. Newlines and tabs stay (code keeps its shape).
pub fn clean_outgoing(text: &str) -> String {
    text.chars()
        .filter(|c| matches!(c, '\n' | '\t') || !crate::text::unsafe_char(*c))
        .collect()
}

/// An id (of a room, an event or a user) as it may be handed out: exactly as it is.
/// `None` for one that is empty, longer than [`MAX_ID`], or that holds what a
/// terminal must not get: an id is compared and parsed by whoever receives it, so one
/// that would have to be changed is not handed out at all.
pub fn clean_id(id: &str) -> Option<String> {
    let fine = !id.is_empty()
        && id.len() <= MAX_ID
        && !id.contains(|c: char| c.is_whitespace() || crate::text::unsafe_char(c));
    fine.then(|| id.to_owned())
}

/// One line of text from outside, safe for a terminal and at most `max` characters;
/// what is cut ends with `…`. For names and reasons, which are shown, never for ids.
pub fn clean_line(text: &str, max: usize) -> String {
    let clean = crate::text::sanitize_line(text);
    match clean.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &clean[..at]),
        None => clean,
    }
}

/// `body` cut to [`MAX_BODY`] bytes at a character, and whether anything was cut.
pub fn bounded_body(mut body: String) -> (String, bool) {
    if body.len() <= MAX_BODY {
        return (body, false);
    }
    let mut end = MAX_BODY;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    body.truncate(end);
    (body, true)
}

// ----- the lines `serve` writes ---------------------------------------------------------

pub fn count_line(count: usize) -> String {
    json!({ "ok": true, "count": count }).to_string()
}

pub fn room_line(room: &RoomInfo) -> String {
    json!({ "room": room }).to_string()
}

pub fn sent_line(event_id: &str) -> String {
    json!({ "ok": true, "event_id": event_id }).to_string()
}

pub fn read_line(room_id: &str, count: usize) -> String {
    json!({ "ok": true, "room": room_id, "count": count }).to_string()
}

pub fn listening_line(room_id: &str) -> String {
    json!({ "ok": true, "room": room_id }).to_string()
}

pub fn message_line(message: &Msg) -> String {
    json!({ "message": message }).to_string()
}

/// A message that was there before, told again by a `listen` because it changed. The
/// field is outside the message, so that a client that does not know it reads the
/// line as ever (and takes the message for new, as before there was the field).
pub fn changed_line(message: &Msg) -> String {
    json!({ "message": message, "changed": true }).to_string()
}

// ----- the lines a client reads ---------------------------------------------------------

/// A header line: its fields if it says `"ok":true`, the refusal if it says
/// `"ok":false`, and `None` if it is neither.
pub fn parse_header(line: &str) -> Option<Result<serde_json::Map<String, Value>, Refusal>> {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    match object.get("ok")?.as_bool()? {
        true => Some(Ok(object)),
        false => {
            let code = object
                .get("error")
                .and_then(Value::as_str)
                .and_then(Code::parse)
                .unwrap_or(Code::Failed);
            let message = object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(Err(Refusal::new(code, clean_line(message, MAX_REASON))))
        }
    }
}

/// What a line after the header holds.
pub enum Item {
    Message(Box<Msg>),
    /// A message that was there before, told again by a `listen` because it changed.
    Changed(Box<Msg>),
    Room(RoomInfo),
    /// The stream has to end.
    End(Refusal),
}

/// A line after the header; `None` if it is none of those.
pub fn parse_item(line: &str) -> Option<Item> {
    let Ok(Value::Object(mut object)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    // A refusal first: it has a "message" too, which is its sentence.
    if object.contains_key("ok") {
        return match parse_header(line)? {
            Err(refusal) => Some(Item::End(refusal)),
            Ok(_) => None,
        };
    }
    if let Some(message) = object.remove("message") {
        let changed = object.get("changed").and_then(Value::as_bool) == Some(true);
        return serde_json::from_value(message).ok().map(|message| {
            if changed {
                Item::Changed(Box::new(message))
            } else {
                Item::Message(Box::new(message))
            }
        });
    }
    let room = object.remove("room")?;
    serde_json::from_value(room).ok().map(Item::Room)
}

// ----- lines, bounded -------------------------------------------------------------------

/// What [`LineReader::next`] found.
#[derive(PartialEq, Eq)]
pub enum Read {
    Line(String),
    /// The other side closed (what it had begun of a line is dropped).
    Eof,
    /// The line is longer than allowed: where the next one starts is unknown, so the
    /// connection is of no more use.
    TooLong,
    /// A line that is not UTF-8.
    Invalid,
    /// Nothing came for as long as the caller waits between lines.
    Idle,
    /// A line was begun and not finished in time.
    Stalled,
}

/// Hand-written: a line is a request or an answer, with rooms and texts in it.
impl std::fmt::Debug for Read {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Line(line) => write!(f, "Line({} bytes)", line.len()),
            Self::Eof => f.write_str("Eof"),
            Self::TooLong => f.write_str("TooLong"),
            Self::Invalid => f.write_str("Invalid"),
            Self::Idle => f.write_str("Idle"),
            Self::Stalled => f.write_str("Stalled"),
        }
    }
}

/// Reads lines of a bounded length from `reader`, keeping at most that length (and one
/// read) in memory. [`next`](Self::next) may be dropped at any await and called again.
pub struct LineReader<R> {
    reader: R,
    buffer: Vec<u8>,
    /// How far the buffer has been searched for the end of the line.
    searched: usize,
    max: usize,
    /// When the line being read began to arrive.
    began: Option<Instant>,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub fn new(reader: R, max: usize) -> Self {
        Self {
            reader,
            buffer: Vec::new(),
            searched: 0,
            max,
            began: None,
        }
    }

    /// The next line, without its end. `idle`: how long to wait for a line to begin
    /// (`None`: for ever). `whole`: how long a line may take from its first byte to
    /// its end (`None`: as long as it likes).
    pub async fn next(
        &mut self,
        idle: Option<Duration>,
        whole: Option<Duration>,
    ) -> std::io::Result<Read> {
        loop {
            if let Some(at) = self.buffer[self.searched..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let end = self.searched + at;
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.searched = 0;
                // What is left is the start of the next line, which begins now.
                self.began = (!self.buffer.is_empty()).then(Instant::now);
                if line.len() > self.max {
                    return Ok(Read::TooLong);
                }
                return Ok(match String::from_utf8(line) {
                    Ok(line) => Read::Line(line),
                    Err(_) => Read::Invalid,
                });
            }
            self.searched = self.buffer.len();
            if self.buffer.len() > self.max {
                return Ok(Read::TooLong);
            }
            let limit = match self.began {
                Some(began) => whole.map(|whole| whole.saturating_sub(began.elapsed())),
                None => idle,
            };
            let mut chunk = [0u8; 4096];
            let read = match limit {
                Some(limit) => {
                    match tokio::time::timeout(limit, self.reader.read(&mut chunk)).await {
                        Ok(read) => read?,
                        Err(_) if self.began.is_some() => return Ok(Read::Stalled),
                        Err(_) => return Ok(Read::Idle),
                    }
                }
                None => self.reader.read(&mut chunk).await?,
            };
            if read == 0 {
                return Ok(Read::Eof);
            }
            if self.began.is_none() {
                self.began = Some(Instant::now());
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    /// Bytes held that are not a whole line yet.
    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.buffer.len()
    }
}

/// Writes `line` and its end, giving up after `limit`: a reader that does not read
/// must not hold the writer for ever.
pub async fn write_line<W: AsyncWrite + Unpin>(
    writer: &mut W,
    line: &str,
    limit: Duration,
) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    let write = async {
        writer.write_all(&bytes).await?;
        writer.flush().await
    };
    match tokio::time::timeout(limit, write).await {
        Ok(result) => result,
        Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> Msg {
        Msg {
            event_id: "$one:example.org".into(),
            sender: "@ann:example.org".into(),
            sender_name: "Ann".into(),
            timestamp_ms: 1_700_000_000_000,
            kind: "text".into(),
            body: "line one\nline \"two\" \\ ü".into(),
            edited: false,
            own: false,
            truncated: false,
            warning: None,
            verified: true,
            untrusted: None,
        }
    }

    /// A message as the worker judges it: `warning` is what the app shows,
    /// `untrusted` the strict judgement.
    fn judged(kind: &str, warning: Option<&str>, untrusted: Option<&str>) -> Msg {
        Msg {
            kind: kind.into(),
            warning: warning.map(str::to_owned),
            verified: untrusted.is_none(),
            untrusted: untrusted.map(str::to_owned),
            ..message()
        }
    }

    #[test]
    fn a_message_counts_as_its_senders_only_when_nothing_is_in_doubt() {
        use Trust::{AllowUnverified, Verified};
        // (the message, strictly, allowing the unverified)
        let cases = [
            // Verified, nothing shown: counts either way.
            (judged("text", None, None), None, None),
            (judged("image", None, None), None, None),
            // Nobody verified the sender: the app shows nothing, strictly it fails.
            (
                judged("text", None, Some("unverified_identity")),
                Some("unverified_identity"),
                None,
            ),
            // A room without encryption.
            (
                judged("text", None, Some("not_encrypted")),
                Some("not_encrypted"),
                None,
            ),
            // The lenient doubts that do not contradict the sender.
            (
                judged("text", Some("unverified_device"), Some("unverified_device")),
                Some("unverified_device"),
                None,
            ),
            (
                judged("text", Some("unconfirmed"), Some("unconfirmed")),
                Some("unconfirmed"),
                None,
            ),
            // What contradicts the sender fails either way.
            (
                judged("text", Some("identity_changed"), Some("identity_changed")),
                Some("identity_changed"),
                Some("identity_changed"),
            ),
            (
                judged("text", Some("sender_mismatch"), Some("sender_mismatch")),
                Some("sender_mismatch"),
                Some("sender_mismatch"),
            ),
            (
                judged("text", Some("not_encrypted"), Some("not_encrypted")),
                Some("not_encrypted"),
                Some("not_encrypted"),
            ),
            // What says nothing never counts, whatever else is claimed about it.
            (
                judged("undecryptable", None, Some("undecryptable")),
                Some("undecryptable"),
                Some("undecryptable"),
            ),
            (
                judged("undecryptable", None, None),
                Some("undecryptable"),
                Some("undecryptable"),
            ),
            (
                judged("redacted", None, Some("redacted")),
                Some("redacted"),
                Some("redacted"),
            ),
            (
                judged("redacted", None, None),
                Some("redacted"),
                Some("redacted"),
            ),
        ];
        for (message, strictly, allowing) in &cases {
            let what = (&message.kind, &message.warning, &message.untrusted);
            assert_eq!(message.doubt(Verified), *strictly, "{what:?}");
            assert_eq!(message.doubt(AllowUnverified), *allowing, "{what:?}");
        }
        // Said to be verified and shown with a warning: the warning wins.
        let odd = Msg {
            verified: true,
            ..judged("text", Some("unconfirmed"), None)
        };
        assert_eq!(odd.doubt(Verified), Some("unconfirmed"));
        // Not verified, and no reason given: still not verified.
        let bare = Msg {
            verified: false,
            ..judged("text", None, None)
        };
        assert_eq!(bare.doubt(Verified), Some("unverified"));

        // Whose word it is: the named sender's only, and never the account's own.
        let ann = judged("text", None, None);
        assert_eq!(ann.standing("@ann:example.org", Verified), Standing::Theirs);
        assert_eq!(ann.standing("@eve:example.org", Verified), Standing::Other);
        assert_eq!(ann.standing("@ann:example.org ", Verified), Standing::Other);
        let own = Msg {
            own: true,
            ..judged("text", None, None)
        };
        assert_eq!(own.standing("@ann:example.org", Verified), Standing::Other);
        assert_eq!(
            own.standing("@ann:example.org", AllowUnverified),
            Standing::Other
        );
        let unverified = judged("text", None, Some("unverified_identity"));
        assert_eq!(
            unverified.standing("@ann:example.org", Verified),
            Standing::Doubted("unverified_identity")
        );
        assert_eq!(
            unverified.standing("@ann:example.org", AllowUnverified),
            Standing::Theirs
        );
        assert_eq!(
            unverified.standing("@eve:example.org", Verified),
            Standing::Other
        );
    }

    #[test]
    fn an_id_is_handed_out_as_it_is_or_not_at_all() {
        for id in [
            "$event:example.org",
            "!room:example.org",
            "@ann:example.org",
            "$a",
        ] {
            assert_eq!(clean_id(id).as_deref(), Some(id));
        }
        let longest = format!("${}", "e".repeat(MAX_ID - 1));
        assert_eq!(clean_id(&longest).as_deref(), Some(longest.as_str()));
        // Too long is not cut (and gets no `…`), and what a terminal must not get is
        // not taken out: such an id is no id.
        let too_long = format!("${}", "e".repeat(MAX_ID));
        for id in [
            "",
            too_long.as_str(),
            "$with space",
            "$line\nbreak",
            "$esc\u{1b}[31m",
            "$flip\u{202e}",
            "$tab\there",
        ] {
            assert_eq!(clean_id(id), None, "{id:?}");
        }
        // A name is shown, so it is cut with a mark.
        assert_eq!(
            clean_line(&"n".repeat(MAX_NAME + 5), MAX_NAME)
                .chars()
                .count(),
            MAX_NAME + 1
        );
    }

    #[test]
    fn requests_survive_the_trip_over_a_line() {
        for request in [
            Request::Rooms,
            Request::Send {
                room: "!room:example.org".into(),
                text: "two\nlines with \"quotes\" and ü".into(),
            },
            Request::Read {
                room: "Agents".into(),
                limit: 100,
            },
            Request::Listen {
                room: "#agents:example.org".into(),
            },
        ] {
            let line = request.line();
            assert!(!line.contains('\n'), "{line}");
            assert!(line.len() <= MAX_REQUEST_LINE);
            assert_eq!(Request::parse(&line), Ok(request));
        }
        // Without a limit a read asks for the usual number; parts that are not known
        // are ignored.
        assert_eq!(
            Request::parse(r#"{"op":"read","room":"R","colour":"blue"}"#),
            Ok(Request::Read {
                room: "R".into(),
                limit: READ_DEFAULT
            })
        );
    }

    #[test]
    fn what_is_not_a_request_is_refused_with_a_reason() {
        let long_room = "r".repeat(MAX_ROOM + 1);
        let long_text = "t".repeat(MAX_TEXT + 1);
        for line in [
            "",
            "hello",
            "[]",
            "42",
            "{}",
            r#"{"op":7}"#,
            r#"{"op":"join","room":"R"}"#,
            r#"{"op":"leave","room":"R"}"#,
            r#"{"op":"create"}"#,
            r#"{"op":"send","room":"R"}"#,
            r#"{"op":"send","text":"hello"}"#,
            r#"{"op":"send","room":"R","text":7}"#,
            r#"{"op":"send","room":"R","text":"   "}"#,
            r#"{"op":"send","room":"","text":"hello"}"#,
            r#"{"op":"send","room":["R"],"text":"hello"}"#,
            r#"{"op":"read"}"#,
            r#"{"op":"read","room":"R","limit":0}"#,
            r#"{"op":"read","room":"R","limit":101}"#,
            r#"{"op":"read","room":"R","limit":-1}"#,
            r#"{"op":"read","room":"R","limit":"many"}"#,
            r#"{"op":"read","room":"R","limit":1.5}"#,
            r#"{"op":"listen"}"#,
            &format!(r#"{{"op":"listen","room":"{long_room}"}}"#),
            &format!(r#"{{"op":"send","room":"R","text":"{long_text}"}}"#),
        ] {
            let refusal = Request::parse(line).expect_err(line);
            assert_eq!(refusal.code, Code::BadRequest, "{line}");
            assert!(!refusal.message.is_empty());
        }
        // A text of exactly the largest size is fine.
        let text = "t".repeat(MAX_TEXT);
        assert!(Request::parse(&format!(r#"{{"op":"send","room":"R","text":"{text}"}}"#)).is_ok());
        // Deep nesting is refused by the JSON reader, not followed.
        let deep = format!("{}{}", "[".repeat(5000), "]".repeat(5000));
        assert!(Request::parse(&deep).is_err());
    }

    #[test]
    fn messages_rooms_and_refusals_survive_the_trip() {
        let sent = message();
        let line = message_line(&sent);
        assert!(!line.contains('\n'), "{line}");
        match parse_item(&line) {
            Some(Item::Message(got)) => assert_eq!(*got, sent),
            _ => panic!("not a message: {line}"),
        }
        // The fields that are only there when they say something.
        assert!(!line.contains("truncated") && !line.contains("warning"));
        assert!(line.contains("\"verified\":true") && !line.contains("untrusted"));
        let doubted = Msg {
            truncated: true,
            warning: Some("sender_mismatch".into()),
            verified: false,
            untrusted: Some("sender_mismatch".into()),
            ..message()
        };
        let line = message_line(&doubted);
        match parse_item(&line) {
            Some(Item::Message(got)) => assert_eq!(*got, doubted),
            _ => panic!("not a message: {line}"),
        }
        // A message told again because it changed says so; a client that does not
        // know that reads the message as ever.
        let line = changed_line(&sent);
        assert!(!line.contains('\n'), "{line}");
        match parse_item(&line) {
            Some(Item::Changed(got)) => assert_eq!(*got, sent),
            _ => panic!("not a changed message: {line}"),
        }
        let Ok(Value::Object(mut object)) = serde_json::from_str::<Value>(&line) else {
            panic!("not an object: {line}");
        };
        let got: Msg = serde_json::from_value(object.remove("message").unwrap()).unwrap();
        assert_eq!(got, sent);
        // Only `true` says so.
        for line in [
            r#"{"message":{"event_id":"$e","sender":"@a:b","sender_name":"a","timestamp_ms":1,"kind":"text","body":"x","edited":false,"own":false},"changed":false}"#,
            r#"{"message":{"event_id":"$e","sender":"@a:b","sender_name":"a","timestamp_ms":1,"kind":"text","body":"x","edited":false,"own":false},"changed":"yes"}"#,
        ] {
            assert!(matches!(parse_item(line), Some(Item::Message(_))), "{line}");
        }
        // A message of a `serve` that does not judge yet is not verified.
        let old = r#"{"message":{"event_id":"$e","sender":"@a:b","sender_name":"a","timestamp_ms":1,"kind":"text","body":"x","edited":false,"own":false}}"#;
        match parse_item(old) {
            Some(Item::Message(got)) => {
                assert!(!got.verified);
                assert_eq!(got.doubt(Trust::Verified), Some("unverified"));
            }
            _ => panic!("not a message"),
        }

        let room = RoomInfo {
            id: "!room:example.org".into(),
            name: "Agents".into(),
            encrypted: true,
            direct: false,
        };
        match parse_item(&room_line(&room)) {
            Some(Item::Room(got)) => assert_eq!(got, room),
            _ => panic!("not a room"),
        }

        for code in Code::ALL {
            let refusal = Refusal::new(code, "why not");
            assert_eq!(parse_header(&refusal.line()), Some(Err(refusal.clone())));
            match parse_item(&refusal.line()) {
                Some(Item::End(got)) => assert_eq!(got, refusal),
                _ => panic!("not an end"),
            }
        }
        // A code from a newer `serve` is still a refusal.
        let unknown = r#"{"ok":false,"error":"brand_new","message":"x"}"#;
        assert_eq!(
            parse_header(unknown),
            Some(Err(Refusal::new(Code::Failed, "x")))
        );

        let header = parse_header(&sent_line("$e:example.org")).unwrap().unwrap();
        assert_eq!(header["event_id"], "$e:example.org");
        let header = parse_header(&read_line("!r:example.org", 3))
            .unwrap()
            .unwrap();
        assert_eq!(
            (header["room"].as_str(), header["count"].as_u64()),
            (Some("!r:example.org"), Some(3))
        );
        assert_eq!(parse_header(&count_line(2)).unwrap().unwrap()["count"], 2);
        assert_eq!(
            parse_header(&listening_line("!r:example.org"))
                .unwrap()
                .unwrap()["room"],
            "!r:example.org"
        );
        for junk in ["", "x", "[]", "{}", r#"{"ok":"yes"}"#, r#"{"message":7}"#] {
            assert!(parse_header(junk).is_none(), "{junk}");
            assert!(parse_item(junk).is_none(), "{junk}");
        }
    }

    #[test]
    fn debug_output_names_no_room_no_text_and_no_reason() {
        let shown = format!(
            "{:?} {:?} {:?} {:?} {:?} {:?}",
            message(),
            RoomInfo {
                id: "!room:example.org".into(),
                name: "Secret Garden".into(),
                encrypted: true,
                direct: false,
            },
            Refusal::failed("Could not send to #garden:example.org"),
            Request::Send {
                room: "Secret Garden".into(),
                text: "the launch codes".into(),
            },
            Request::Listen {
                room: "!room:example.org".into(),
            },
            Read::Line(r#"{"op":"listen","room":"Secret Garden"}"#.into()),
        );
        for private in ["example.org", "Ann", "line one", "Garden", "launch", "$one"] {
            assert!(!shown.contains(private), "{private} in {shown}");
        }
    }

    #[test]
    fn text_is_cleaned_and_cut() {
        // Out to Matrix: control characters go, newlines and tabs stay.
        assert_eq!(
            clean_outgoing("a\u{1b}[31mb\n\tc\u{202e}d\u{0}"),
            "a[31mb\n\tcd"
        );
        assert!(check_text("\u{1b}\u{7}").is_err());
        assert!(check_text("x").is_ok());
        // A reason: one line, cut by characters.
        assert_eq!(clean_line("a\nb\u{1b}c", 10), "a bc");
        assert_eq!(clean_line("üüüüü", 3), "üüü…");
        assert_eq!(
            Refusal::failed(&"x".repeat(1000)).message.chars().count(),
            MAX_REASON + 1
        );
        // A body: cut by bytes, at a character.
        let (body, cut) = bounded_body("ü".repeat(MAX_BODY));
        assert!(cut);
        assert_eq!(body.len(), MAX_BODY);
        assert!(body.chars().all(|c| c == 'ü'));
        let (body, cut) = bounded_body(format!("a{}", "ü".repeat(MAX_BODY / 2)));
        assert!(cut);
        assert_eq!(body.len(), MAX_BODY - 1);
        assert_eq!(bounded_body("short".into()), ("short".to_owned(), false));
        // The longest body, escaped at its worst, still fits a reply line.
        let worst = Msg {
            body: "\"".repeat(MAX_BODY),
            sender_name: "\"".repeat(MAX_NAME),
            ..message()
        };
        assert!(message_line(&worst).len() < MAX_REPLY_LINE);
        assert!(changed_line(&worst).len() < MAX_REPLY_LINE);
    }

    #[tokio::test]
    async fn lines_are_split_and_a_long_one_is_refused_without_being_kept() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut lines = LineReader::new(server, 32);
        let writer = tokio::spawn(async move {
            client.write_all(b"one\r\ntwo\n\nthr").await.unwrap();
            client.write_all(b"ee\n\xff\xfe\n").await.unwrap();
            // Longer than allowed, and never ended.
            let _ = client.write_all(&[b'x'; 100_000]).await;
        });
        let whole = Some(Duration::from_secs(5));
        for expected in ["one", "two", "", "three"] {
            assert_eq!(
                lines.next(whole, whole).await.unwrap(),
                Read::Line(expected.to_owned())
            );
        }
        assert_eq!(lines.next(whole, whole).await.unwrap(), Read::Invalid);
        assert_eq!(lines.next(whole, whole).await.unwrap(), Read::TooLong);
        // No more than the largest line and one read was ever held.
        assert!(lines.held() <= 32 + 4096, "{}", lines.held());
        writer.abort();
    }

    #[tokio::test]
    async fn a_line_that_is_not_finished_and_a_silent_peer_are_told_apart() {
        let (mut client, server) = tokio::io::duplex(64);
        let mut lines = LineReader::new(server, 1024);
        let short = Some(Duration::from_millis(40));
        // Nothing at all: idle.
        assert_eq!(lines.next(short, short).await.unwrap(), Read::Idle);
        // Half a line, then silence: stalled, however the bytes trickle in.
        client.write_all(b"{\"op\":").await.unwrap();
        let trickle = tokio::spawn(async move {
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if client.write_all(b" ").await.is_err() {
                    break;
                }
            }
            client
        });
        assert_eq!(
            lines
                .next(
                    Some(Duration::from_secs(30)),
                    Some(Duration::from_millis(80))
                )
                .await
                .unwrap(),
            Read::Stalled
        );
        assert!(!trickle.is_finished(), "the trickle kept the line alive");
        // The other side closing ends it.
        drop(trickle.await.unwrap());
        assert_eq!(lines.next(None, None).await.unwrap(), Read::Eof);
    }

    #[tokio::test]
    async fn a_write_to_a_peer_that_does_not_read_gives_up() {
        let (mut writer, _reader) = tokio::io::duplex(16);
        let line = "x".repeat(1000);
        let began = std::time::Instant::now();
        let outcome = write_line(&mut writer, &line, Duration::from_millis(50)).await;
        assert_eq!(outcome.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(began.elapsed() < Duration::from_secs(2));
        // A short line to a reader that reads arrives whole, with its end.
        let (mut writer, mut reader) = tokio::io::duplex(64);
        write_line(&mut writer, "hello", Duration::from_secs(1))
            .await
            .unwrap();
        let mut got = [0u8; 6];
        reader.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello\n");
    }
}
