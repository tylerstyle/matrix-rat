//! The client's end of the agent socket, and the commands `send`, `read` and `listen`.
//!
//! A client never opens the profile's store or its lock, and makes no folder: it
//! connects to the socket of a running `matrix-rat serve`, or says that none runs.
//! It checks that the other end is a program of this user, bounds every line it
//! reads, and waits for an answer with a limit.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use super::proto::{
    self, Item, LineReader, Msg, Read, Refusal, Request, RoomInfo, Standing, Trust,
};
use super::socket;

/// How long a client waits for the first line of an answer. `serve` answers or
/// refuses well before (see `serve::Limits`).
const ANSWER: Duration = Duration::from_secs(300);
/// How long a line of an answer may take from its first byte to its end.
const LINE: Duration = Duration::from_secs(30);
/// How long a request may wait to be taken.
const WRITE: Duration = Duration::from_secs(10);

/// What a command ends with, for the shell.
pub mod exit {
    pub const OK: u8 = 0;
    /// Something went wrong on the way (the connection broke, an answer that cannot
    /// be read).
    pub const FAILED: u8 = 1;
    /// The command line makes no sense. (Also what the argument parser ends with.)
    pub const USAGE: u8 = 2;
    /// No `matrix-rat serve` is running for the profile.
    pub const NOT_RUNNING: u8 = 3;
    /// `serve` refused; its reason is printed.
    pub const REFUSED: u8 = 4;
}

#[derive(Debug)]
pub enum ClientError {
    /// Nobody answers on the profile's socket.
    NotRunning,
    /// `serve` said no.
    Refused(Refusal),
    /// The other end is not what it should be, or went away.
    Protocol(&'static str),
    Io(std::io::Error),
}

impl ClientError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::NotRunning => exit::NOT_RUNNING,
            Self::Refused(_) => exit::REFUSED,
            Self::Protocol(_) | Self::Io(_) => exit::FAILED,
        }
    }

    /// What the user is told; `profile` is the user's own word for the account.
    pub fn message(&self, profile: &str) -> String {
        match self {
            Self::NotRunning => format!(
                "no `matrix-rat serve` is running for the profile \"{profile}\". Start it \
                 with: matrix-rat --profile {profile} serve"
            ),
            Self::Refused(refusal) => {
                format!("refused ({}): {}", refusal.code.as_str(), refusal.message)
            }
            Self::Protocol(what) => (*what).to_owned(),
            Self::Io(err) => format!("the connection to matrix-rat serve failed: {err}"),
        }
    }
}

/// A connection to a running `serve`.
pub struct Conn {
    lines: LineReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Conn {
    pub async fn open(socket: &Path) -> Result<Self, ClientError> {
        let stream = match UnixStream::connect(socket).await {
            Ok(stream) => stream,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(ClientError::NotRunning);
            }
            Err(err) => return Err(ClientError::Io(err)),
        };
        // The folder is ours alone, so this cannot be anyone else; asked all the same.
        if !socket::peer_is_us(&stream) {
            return Err(ClientError::Protocol(
                "the socket is served by another user; not used",
            ));
        }
        let (reader, writer) = stream.into_split();
        Ok(Self {
            lines: LineReader::new(reader, proto::MAX_REPLY_LINE),
            writer,
        })
    }

    /// The next line of an answer; `idle`: how long to wait for it to begin.
    async fn line(&mut self, idle: Option<Duration>) -> Result<String, ClientError> {
        match self
            .lines
            .next(idle, Some(LINE))
            .await
            .map_err(ClientError::Io)?
        {
            Read::Line(line) => Ok(line),
            Read::Eof => Err(ClientError::Protocol(
                "matrix-rat serve closed the connection",
            )),
            Read::TooLong | Read::Invalid => Err(ClientError::Protocol(
                "matrix-rat serve sent an answer that cannot be read",
            )),
            Read::Idle | Read::Stalled => Err(ClientError::Protocol(
                "matrix-rat serve did not answer in time",
            )),
        }
    }

    /// Sends `request` and reads the header of its answer.
    async fn ask(&mut self, request: &Request) -> Result<Map<String, Value>, ClientError> {
        let wrote = proto::write_line(&mut self.writer, &request.line(), WRITE).await;
        // A `serve` that has no room for us says so and closes, which can make the
        // write fail: what it said is still there to be read.
        let idle = match &wrote {
            Ok(()) => ANSWER,
            Err(_) => Duration::from_secs(1),
        };
        let header = match self.line(Some(idle)).await {
            Ok(line) => proto::parse_header(&line),
            Err(err) => {
                return Err(match wrote {
                    Ok(()) => err,
                    Err(err) => ClientError::Io(err),
                });
            }
        };
        match header {
            Some(Ok(header)) => Ok(header),
            Some(Err(refusal)) => Err(ClientError::Refused(refusal)),
            None => Err(ClientError::Protocol(
                "matrix-rat serve sent an answer that cannot be read",
            )),
        }
    }

    /// The lines that follow a header that announced `count` of them.
    async fn items(
        &mut self,
        header: &Map<String, Value>,
        most: usize,
    ) -> Result<Vec<Item>, ClientError> {
        let count = header
            .get("count")
            .and_then(Value::as_u64)
            .filter(|count| *count <= most as u64)
            .ok_or(ClientError::Protocol(
                "matrix-rat serve sent an answer that cannot be read",
            ))? as usize;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            let line = self.line(Some(ANSWER)).await?;
            match proto::parse_item(&line) {
                Some(Item::End(refusal)) => return Err(ClientError::Refused(refusal)),
                Some(item) => items.push(item),
                None => {
                    return Err(ClientError::Protocol(
                        "matrix-rat serve sent an answer that cannot be read",
                    ));
                }
            }
        }
        Ok(items)
    }

    /// The rooms the account is in.
    pub async fn rooms(&mut self) -> Result<Vec<RoomInfo>, ClientError> {
        let header = self.ask(&Request::Rooms).await?;
        let items = self.items(&header, proto::MAX_ROOMS).await?;
        Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Item::Room(room) => Some(room),
                _ => None,
            })
            .collect())
    }

    /// Sends `text` to `room`; the id of the event it became.
    pub async fn send(&mut self, room: &str, text: &str) -> Result<String, ClientError> {
        let header = self
            .ask(&Request::Send {
                room: room.to_owned(),
                text: text.to_owned(),
            })
            .await?;
        header
            .get("event_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(ClientError::Protocol(
                "matrix-rat serve sent an answer that cannot be read",
            ))
    }

    /// The latest `limit` messages of `room`, oldest first, with the room's id.
    pub async fn read(
        &mut self,
        room: &str,
        limit: usize,
    ) -> Result<(String, Vec<Msg>), ClientError> {
        let header = self
            .ask(&Request::Read {
                room: room.to_owned(),
                limit,
            })
            .await?;
        let room_id = room_of(&header)?;
        let items = self.items(&header, proto::READ_MAX).await?;
        let messages = items
            .into_iter()
            .filter_map(|item| match item {
                Item::Message(message) => Some(*message),
                _ => None,
            })
            .collect();
        Ok((room_id, messages))
    }

    /// Listens to `room`: when this returns, everything that happens in the room
    /// from now on comes through the stream.
    pub async fn listen(mut self, room: &str) -> Result<Listener, ClientError> {
        let header = self
            .ask(&Request::Listen {
                room: room.to_owned(),
            })
            .await?;
        Ok(Listener {
            room_id: room_of(&header)?,
            conn: self,
        })
    }
}

fn room_of(header: &Map<String, Value>) -> Result<String, ClientError> {
    header
        .get("room")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(ClientError::Protocol(
            "matrix-rat serve sent an answer that cannot be read",
        ))
}

/// A room being listened to.
pub struct Listener {
    conn: Conn,
    room_id: String,
}

impl Listener {
    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    /// The next message that is new or has changed; waits for as long as it takes.
    /// An error is the end of the stream. May be dropped while waiting and called
    /// again.
    pub async fn next(&mut self) -> Result<Msg, ClientError> {
        loop {
            let line = self.conn.line(None).await?;
            match proto::parse_item(&line) {
                Some(Item::Message(message)) => return Ok(*message),
                Some(Item::End(refusal)) => return Err(ClientError::Refused(refusal)),
                // A line of a kind a newer `serve` may send.
                _ => {}
            }
        }
    }
}

// ----- the commands ---------------------------------------------------------------------

/// `--from`, and how much it takes for a message to count as that user's.
#[derive(Debug, Clone, Copy)]
pub struct Sender<'a> {
    pub user: &'a str,
    pub trust: Trust,
}

/// What a message is to a command: printed without `--from`; with it, only what
/// counts as that user's, while what names the user and does not count is told apart
/// from what is someone else's.
fn standing<'a>(message: &'a Msg, from: Option<Sender<'_>>) -> Standing<'a> {
    match from {
        None => Standing::Theirs,
        Some(sender) => message.standing(sender.user, sender.trust),
    }
}

/// Checks a `--room` argument; the complaint is for the user.
pub fn check_room_arg(room: &str) -> Result<(), String> {
    proto::check_room(room).map_err(|_| {
        format!(
            "--room needs a room id, a room name or a room address (at most {} bytes)",
            proto::MAX_ROOM
        )
    })
}

/// Checks a `--from` argument.
pub fn check_from_arg(from: &str) -> Result<(), String> {
    match matrix_sdk::ruma::UserId::parse(from) {
        Ok(_) if from.starts_with('@') => Ok(()),
        _ => Err("--from needs a full Matrix user id, like @you:example.org".to_owned()),
    }
}

/// The text of a `send`: the words of the command line, or with `-` what standard
/// input holds (without the line end a shell adds).
pub fn text_arg(words: &[String], stdin: &mut impl std::io::Read) -> Result<String, String> {
    use std::io::Read as _;
    let text = if words.len() == 1 && words[0] == "-" {
        let mut bytes = Vec::new();
        stdin
            .take(proto::MAX_TEXT as u64 + 2)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("could not read the text from standard input: {err}"))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| "the text on standard input is not UTF-8".to_owned())?;
        text.strip_suffix('\n').unwrap_or(&text).to_owned()
    } else {
        words.join(" ")
    };
    proto::check_text(&text).map_err(|_| {
        format!(
            "send needs some text, at most {} bytes (as words, or `-` to read it \
             from standard input)",
            proto::MAX_TEXT
        )
    })?;
    Ok(text)
}

/// `matrix-rat rooms`: prints the rooms the account is in, a line of JSON each.
pub async fn rooms(socket: &Path, out: &mut impl Write) -> Result<(), ClientError> {
    for room in Conn::open(socket).await?.rooms().await? {
        let line = serde_json::to_string(&room)
            .map_err(|_| ClientError::Protocol("a room that cannot be written"))?;
        print_line(out, &line)?;
    }
    Ok(())
}

/// `matrix-rat send`: prints the id of the sent event as a line of JSON.
pub async fn send(
    socket: &Path,
    room: &str,
    text: &str,
    out: &mut impl Write,
) -> Result<(), ClientError> {
    let event_id = Conn::open(socket).await?.send(room, text).await?;
    let line = serde_json::json!({ "event_id": event_id }).to_string();
    print_line(out, &line)
}

/// `matrix-rat read`: prints the latest `limit` messages, oldest first, a line of
/// JSON each. With `from`: of what counts as that user's among the latest
/// [`proto::READ_MAX`] of the room; if messages that name the user were left out
/// because they do not count, `notes` (standard error) is told how many, without a
/// word of what they say.
pub async fn read(
    socket: &Path,
    room: &str,
    limit: usize,
    from: Option<Sender<'_>>,
    out: &mut impl Write,
    notes: &mut impl Write,
) -> Result<(), ClientError> {
    let asked = if from.is_some() {
        proto::READ_MAX
    } else {
        limit
    };
    let (_, messages) = Conn::open(socket).await?.read(room, asked).await?;
    let mut kept: Vec<&Msg> = Vec::new();
    let mut left_out = 0usize;
    for message in &messages {
        match standing(message, from) {
            Standing::Theirs => kept.push(message),
            Standing::Doubted(_) => left_out += 1,
            Standing::Other => {}
        }
    }
    for message in &kept[kept.len().saturating_sub(limit)..] {
        print_message(out, message)?;
    }
    if left_out > 0 {
        let _ = writeln!(
            notes,
            "matrix-rat: {left_out} of the messages that name this user as sender were left \
             out: they could not be verified as theirs (read without --from to see them \
             and why; --allow-unverified takes them)"
        );
    }
    Ok(())
}

/// `matrix-rat listen`: prints every message that is new or has changed, a line of
/// JSON each, until the stream ends or nobody reads the output any more. With `from`:
/// only what counts as that user's; a message that names the user and does not count
/// is noted on `notes` (standard error) with the reason and nothing of what it says.
pub async fn listen(
    socket: &Path,
    room: &str,
    from: Option<Sender<'_>>,
    out: &mut impl Write,
    notes: &mut impl Write,
) -> Result<(), ClientError> {
    let mut listener = Conn::open(socket).await?.listen(room).await?;
    loop {
        let message = listener.next().await?;
        match standing(&message, from) {
            Standing::Theirs => match print_message(out, &message) {
                // `listen … | head -1`: done.
                Err(ClientError::Io(err)) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                    return Ok(());
                }
                other => other?,
            },
            Standing::Doubted(why) => {
                let _ = writeln!(
                    notes,
                    "matrix-rat: a message that names this user as sender was left out: {why}"
                );
            }
            Standing::Other => {}
        }
    }
}

fn print_message(out: &mut impl Write, message: &Msg) -> Result<(), ClientError> {
    let line = serde_json::to_string(message)
        .map_err(|_| ClientError::Protocol("a message that cannot be written"))?;
    print_line(out, &line)
}

fn print_line(out: &mut impl Write, line: &str) -> Result<(), ClientError> {
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(ClientError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(sender: &str, own: bool) -> Msg {
        Msg {
            event_id: "$e".into(),
            sender: sender.into(),
            sender_name: "name".into(),
            timestamp_ms: 1,
            kind: "text".into(),
            body: "hello".into(),
            edited: false,
            own,
            truncated: false,
            warning: None,
            verified: true,
            untrusted: None,
        }
    }

    #[test]
    fn from_keeps_what_counts_as_that_users_and_never_the_accounts_own() {
        let strict = |user| {
            Some(Sender {
                user,
                trust: Trust::Verified,
            })
        };
        let lax = |user| {
            Some(Sender {
                user,
                trust: Trust::AllowUnverified,
            })
        };
        let ann = message("@ann:example.org", false);
        let bob = message("@bob:example.org", false);
        let own = message("@bot:example.org", true);
        // Without `--from` everything is printed.
        for message in [&ann, &bob, &own] {
            assert_eq!(standing(message, None), Standing::Theirs);
        }
        assert_eq!(standing(&ann, strict("@ann:example.org")), Standing::Theirs);
        assert_eq!(standing(&bob, strict("@ann:example.org")), Standing::Other);
        assert_eq!(standing(&own, strict("@ann:example.org")), Standing::Other);
        // Naming the account itself gives nothing rather than everything of its own.
        assert_eq!(standing(&own, strict("@bot:example.org")), Standing::Other);
        assert_eq!(standing(&own, lax("@bot:example.org")), Standing::Other);
        // Not verified: left out unless that is allowed.
        let unverified = Msg {
            verified: false,
            untrusted: Some("unverified_identity".into()),
            ..message("@ann:example.org", false)
        };
        assert_eq!(
            standing(&unverified, strict("@ann:example.org")),
            Standing::Doubted("unverified_identity")
        );
        assert_eq!(
            standing(&unverified, lax("@ann:example.org")),
            Standing::Theirs
        );
        // What contradicts the sender is left out either way.
        let forged = Msg {
            warning: Some("sender_mismatch".into()),
            verified: false,
            untrusted: Some("sender_mismatch".into()),
            ..message("@ann:example.org", false)
        };
        for from in [strict("@ann:example.org"), lax("@ann:example.org")] {
            assert_eq!(
                standing(&forged, from),
                Standing::Doubted("sender_mismatch")
            );
        }
    }

    /// `read --from` and `listen --from` against a `serve`: only what counts as the
    /// user's reaches the output; what names the user and does not count is told on
    /// the side, by number or reason, never by its text.
    #[tokio::test]
    async fn read_and_listen_with_from_print_only_what_counts_as_the_users() {
        use crate::agent::testing::{ANN, EVE, ROOM, ROOM_NAME, World, message, own};
        let doubted = |id: &str, kind: &str, warning: Option<&str>, why: &str| Msg {
            kind: kind.into(),
            warning: warning.map(str::to_owned),
            verified: false,
            untrusted: Some(why.into()),
            ..message(id, ANN, "SECRET-TEXT of a message that does not count")
        };
        let history = vec![
            message("$1", ANN, "one"),
            message("$2", EVE, "I am Ann"),
            own("$3", "[agent] hello"),
            doubted("$4", "text", None, "unverified_identity"),
            doubted("$5", "text", Some("unverified_device"), "unverified_device"),
            doubted("$6", "text", Some("sender_mismatch"), "sender_mismatch"),
            doubted("$7", "undecryptable", None, "undecryptable"),
            doubted("$8", "redacted", None, "redacted"),
            message("$9", ANN, "nine"),
        ];
        let world = World::start("cli-from", history).await;
        let socket = world.socket();
        let ids = |out: &[u8]| -> Vec<String> {
            String::from_utf8(out.to_vec())
                .unwrap()
                .lines()
                .map(|line| {
                    let message: Msg = serde_json::from_str(line).expect("a message");
                    message.event_id
                })
                .collect()
        };
        let from = |trust| Some(Sender { user: ANN, trust });

        // Without `--from`: everything, with what is known about each.
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        read(&socket, ROOM_NAME, 100, None, &mut out, &mut notes)
            .await
            .unwrap();
        assert_eq!(ids(&out).len(), 9);
        assert!(notes.is_empty());
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("\"untrusted\":\"unverified_identity\""),
            "{printed}"
        );

        // Strictly: only the verified ones of the user; five were left out, and
        // nothing of what they say is told.
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        read(
            &socket,
            ROOM,
            20,
            from(Trust::Verified),
            &mut out,
            &mut notes,
        )
        .await
        .unwrap();
        assert_eq!(ids(&out), ["$1", "$9"]);
        let told = String::from_utf8(notes).unwrap();
        assert!(told.starts_with("matrix-rat: 5 of the messages"), "{told}");
        assert!(told.contains("--allow-unverified"), "{told}");
        assert!(!told.contains("SECRET-TEXT"), "{told}");
        // The limit counts what is printed.
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        read(
            &socket,
            ROOM,
            1,
            from(Trust::Verified),
            &mut out,
            &mut notes,
        )
        .await
        .unwrap();
        assert_eq!(ids(&out), ["$9"]);

        // Allowing the unverified: also what nobody verified, but never what
        // contradicts the sender, could not be read or was deleted.
        let (mut out, mut notes) = (Vec::new(), Vec::new());
        read(
            &socket,
            ROOM,
            20,
            from(Trust::AllowUnverified),
            &mut out,
            &mut notes,
        )
        .await
        .unwrap();
        assert_eq!(ids(&out), ["$1", "$4", "$5", "$9"]);
        let told = String::from_utf8(notes).unwrap();
        assert!(told.starts_with("matrix-rat: 3 of the messages"), "{told}");

        // Listening: the same judgement, message by message.
        let listening = tokio::spawn({
            let socket = socket.clone();
            async move {
                let (mut out, mut notes) = (HeadOf(Vec::new(), 2), Vec::new());
                let ended =
                    listen(&socket, ROOM, from(Trust::Verified), &mut out, &mut notes).await;
                (ended, out.0, notes)
            }
        });
        // Watched: whatever is posted from here on is news to the listener.
        world.until_watched(true).await;
        world.post(message("$a", EVE, "not her"));
        world.post(doubted("$b", "undecryptable", None, "undecryptable"));
        world.post(doubted("$c", "text", None, "unverified_identity"));
        world.post(message("$d", ANN, "yes"));
        // Decrypted at last, into something that counts.
        world.change("$b", |message| {
            *message = crate::agent::testing::message("$b", ANN, "readable now");
        });
        // One more than the output takes: the command ends.
        world.post(message("$e", ANN, "more"));
        let (ended, out, notes) = tokio::time::timeout(Duration::from_secs(10), listening)
            .await
            .expect("listen ends when nobody reads its output any more")
            .unwrap();
        assert!(ended.is_ok());
        assert_eq!(ids(&out), ["$d", "$b"]);
        let told = String::from_utf8(notes).unwrap();
        assert_eq!(
            told.lines().collect::<Vec<_>>(),
            [
                "matrix-rat: a message that names this user as sender was left out: undecryptable",
                "matrix-rat: a message that names this user as sender was left out: unverified_identity",
            ]
        );
        assert!(world.odd().is_empty());
        world.stop().await;
    }

    /// An output that takes a number of lines and then is closed, like `| head -n`.
    struct HeadOf(Vec<u8>, usize);

    impl Write for HeadOf {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.1 == 0 {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.1 -= bytes
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                .min(self.1);
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn arguments_are_checked_before_anything_is_sent() {
        assert!(check_room_arg("Agents").is_ok());
        assert!(check_room_arg("!room:example.org").is_ok());
        assert!(check_room_arg("  ").is_err());
        assert!(check_room_arg(&"r".repeat(proto::MAX_ROOM + 1)).is_err());
        assert!(check_from_arg("@you:example.org").is_ok());
        for bad in ["", "you", "you:example.org", "@you", "#room:example.org"] {
            assert!(check_from_arg(bad).is_err(), "{bad}");
        }

        let words =
            |words: &[&str]| -> Vec<String> { words.iter().map(|w| (*w).to_owned()).collect() };
        let mut none = std::io::empty();
        assert_eq!(
            text_arg(&words(&["two", "words"]), &mut none).unwrap(),
            "two words"
        );
        assert!(text_arg(&[], &mut none).is_err());
        assert!(text_arg(&words(&["  "]), &mut none).is_err());
        // A dash among words is a dash.
        assert_eq!(text_arg(&words(&["a", "-"]), &mut none).unwrap(), "a -");
        // Alone it means standard input, whose last line end is the shell's.
        let mut piped = "line one\nline two\n".as_bytes();
        assert_eq!(
            text_arg(&words(&["-"]), &mut piped).unwrap(),
            "line one\nline two"
        );
        let mut empty = "\n".as_bytes();
        assert!(text_arg(&words(&["-"]), &mut empty).is_err());
        let mut binary: &[u8] = &[0xff, 0xfe];
        assert!(text_arg(&words(&["-"]), &mut binary).is_err());
        // Too long is refused, and no more than that is read.
        let long = "x".repeat(proto::MAX_TEXT * 4);
        let mut source = long.as_bytes();
        assert!(text_arg(&words(&["-"]), &mut source).is_err());
        assert!(source.len() >= proto::MAX_TEXT * 2, "read too far");
        let fits = format!("{}\n", "x".repeat(proto::MAX_TEXT));
        assert_eq!(
            text_arg(&words(&["-"]), &mut fits.as_bytes())
                .unwrap()
                .len(),
            proto::MAX_TEXT
        );
    }

    #[test]
    fn every_failure_has_its_exit_code_and_says_what_to_do() {
        let not_running = ClientError::NotRunning;
        assert_eq!(not_running.exit_code(), 3);
        let told = not_running.message("bot");
        assert!(told.contains("matrix-rat --profile bot serve"), "{told}");
        let refused = ClientError::Refused(Refusal::new(proto::Code::Ambiguous, "ambiguous: x"));
        assert_eq!(refused.exit_code(), 4);
        assert!(refused.message("bot").contains("ambiguous"));
        assert_eq!(ClientError::Protocol("odd").exit_code(), 1);
        assert_eq!(
            ClientError::Io(std::io::ErrorKind::BrokenPipe.into()).exit_code(),
            1
        );
        assert_eq!((exit::OK, exit::USAGE), (0, 2));
    }

    #[tokio::test]
    async fn without_a_serve_the_commands_say_so_and_make_nothing() {
        let dir = crate::agent::testing::Dir::new("none");
        // No socket at all, in a folder that is not there either.
        let socket = dir.path().join("run").join("bot.sock");
        let mut out = Vec::new();
        assert!(matches!(
            rooms(&socket, &mut out).await,
            Err(ClientError::NotRunning)
        ));
        assert!(matches!(
            send(&socket, "Agents", "hello", &mut out).await,
            Err(ClientError::NotRunning)
        ));
        let mut notes = Vec::new();
        assert!(matches!(
            read(&socket, "Agents", 5, None, &mut out, &mut notes).await,
            Err(ClientError::NotRunning)
        ));
        assert!(matches!(
            listen(&socket, "Agents", None, &mut out, &mut notes).await,
            Err(ClientError::NotRunning)
        ));
        assert!(out.is_empty() && notes.is_empty());
        assert!(!dir.path().join("run").exists(), "a client makes no folder");
        // A socket that a killed `serve` left behind: nobody answers there.
        let stale = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        assert!(matches!(
            Conn::open(&stale).await,
            Err(ClientError::NotRunning)
        ));
        // The client does not remove it: that is for the next `serve`.
        assert!(stale.exists());
    }
}
