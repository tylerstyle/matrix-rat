//! `serve` with the test in the worker's place: a pair of channels instead of the SDK,
//! and a real socket in a folder of the test's own.

use std::path::Path;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc::unbounded_channel;

use super::*;
use crate::agent::client::{ClientError, Conn, Listener};
use crate::agent::proto::Item;
use crate::agent::testing::{ANN, BOT, Dir, EVE, Fake, ROOM, ROOM_NAME, message, own, quick, room};

/// The refusal a request ended with.
fn refusal<T>(result: Result<T, ClientError>) -> Refusal {
    match result {
        Err(ClientError::Refused(refusal)) => refusal,
        Err(other) => panic!("not a refusal: {other:?}"),
        Ok(_) => panic!("not refused"),
    }
}

async fn next(listener: &mut Listener) -> Result<Msg, ClientError> {
    tokio::time::timeout(Duration::from_secs(10), listener.next())
        .await
        .expect("the stream says something in time")
}

/// A listener on the room, with `there` as what the room held before it listened.
async fn listening(fake: &mut Fake, there: Vec<Msg>) -> Listener {
    let socket = fake.socket.clone();
    let (listener, ()) = tokio::join!(
        async move { Conn::open(&socket).await.unwrap().listen(ROOM).await },
        async {
            match fake.asked().await {
                AgentCmd::Watch { room_id } => assert_eq!(room_id, ROOM),
                other => panic!("not a watch: {other:?}"),
            }
            fake.watched(there);
        }
    );
    listener.expect("listening")
}

/// A connection that speaks the protocol by hand.
struct Raw {
    lines: LineReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl Raw {
    async fn open(socket: &Path) -> Self {
        let (reader, writer) = UnixStream::connect(socket).await.unwrap().into_split();
        Self {
            lines: LineReader::new(reader, proto::MAX_REPLY_LINE),
            writer,
        }
    }

    async fn say(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn hear(&mut self) -> Read {
        tokio::time::timeout(Duration::from_secs(10), self.lines.next(None, None))
            .await
            .expect("an answer in time")
            .unwrap_or(Read::Eof)
    }

    async fn line(&mut self) -> String {
        match self.hear().await {
            Read::Line(line) => line,
            other => panic!("not a line: {other:?}"),
        }
    }

    /// The refusal the next line holds.
    async fn refused(&mut self) -> Refusal {
        let line = self.line().await;
        match proto::parse_header(&line) {
            Some(Err(refusal)) => refusal,
            _ => panic!("not a refusal: {line}"),
        }
    }
}

#[tokio::test]
async fn rooms_lists_what_the_account_has_joined_and_nothing_it_was_invited_to() {
    let mut fake = Fake::start("rooms", quick()).await;
    // Asked before the worker has told the rooms: the answer waits for them.
    let socket = fake.socket.clone();
    let early = tokio::spawn(async move { Conn::open(&socket).await.unwrap().rooms().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!early.is_finished());

    let mut direct = room("!ann:example.org", "Ann");
    direct.direct = true;
    direct.encrypted = false;
    direct.category = RoomCategory::Person;
    let mut invite = room("!invite:example.org", "Come in");
    invite.category = RoomCategory::Invite;
    let mut knock = room("!knock:example.org", "Let me in");
    knock.category = RoomCategory::Knock;
    let nasty = room("!nasty:example.org", "bad\u{1b}[31m\nname");
    fake.tell(MatrixEvent::Rooms(vec![
        room(ROOM, ROOM_NAME),
        direct,
        invite,
        knock,
        nasty,
    ]));
    let rooms = early.await.unwrap().unwrap();
    let listed: Vec<(&str, &str, bool, bool)> = rooms
        .iter()
        .map(|room| {
            (
                room.id.as_str(),
                room.name.as_str(),
                room.encrypted,
                room.direct,
            )
        })
        .collect();
    assert_eq!(
        listed,
        [
            (ROOM, ROOM_NAME, true, false),
            ("!ann:example.org", "Ann", false, true),
            ("!nasty:example.org", "bad[31m name", true, false),
        ]
    );
    // The invitation stays an invitation: nothing was asked of the worker at all.
    assert!(fake.asked_nothing());
    fake.stop().await;
}

#[tokio::test]
async fn without_the_rooms_a_request_is_refused_after_a_while() {
    let limits = Limits {
        ready: Duration::from_millis(100),
        ..quick()
    };
    let mut fake = Fake::start("notready", limits).await;
    let mut conn = Conn::open(&fake.socket).await.unwrap();
    assert_eq!(refusal(conn.send(ROOM, "hello").await).code, Code::NotReady);
    assert!(fake.asked_nothing());
    // Once they are there, the same connection is served.
    fake.tell(MatrixEvent::Rooms(vec![room(ROOM, ROOM_NAME)]));
    assert_eq!(conn.rooms().await.unwrap().len(), 1);
    fake.stop().await;
}

#[tokio::test]
async fn send_finds_the_room_and_tells_what_became_of_the_message() {
    let mut fake = Fake::start("send", quick()).await;
    let mut addressed = room("!addr:example.org", "Lobby");
    addressed.alias = Some("#lobby:example.org".into());
    let mut invite = room("!invite:example.org", "Party");
    invite.category = RoomCategory::Invite;
    // A room whose address is what another room is called.
    let mut hall = room("!hall:example.org", "Hall");
    hall.alias = Some("#hall:example.org".into());
    // Rooms whose ids cannot be handed out as they are: too long, or not clean.
    let long_id = format!("!{}:example.org", "x".repeat(300));
    fake.tell(MatrixEvent::Rooms(vec![
        room(ROOM, ROOM_NAME),
        addressed,
        room("!twin1:example.org", "Twins"),
        room("!twin2:example.org", "Twins"),
        invite,
        hall,
        room("!nothall:example.org", "#hall:example.org"),
        // Named after the id of another room, to be taken for it.
        room("!copycat:example.org", ROOM),
        room(&long_id, "Long"),
        room("!bad\u{1b}[31m:example.org", "Unclean"),
    ]));
    let mut conn = Conn::open(&fake.socket).await.unwrap();
    // Listed are the rooms that can be named: an id is never cut or cleaned.
    let listed = conn.rooms().await.unwrap();
    assert_eq!(listed.len(), 7);
    assert!(
        listed
            .iter()
            .all(|room| room.id.len() < 40 && !room.id.contains('…'))
    );

    // By name, by id, by address; spaces around a name do not matter.
    for (asked, expected) in [
        (ROOM_NAME, ROOM),
        (ROOM, ROOM),
        ("#lobby:example.org", "!addr:example.org"),
        ("Lobby", "!addr:example.org"),
        ("  Agents ", ROOM),
    ] {
        let text = "**done** with /etc/hosts";
        let (sent, ()) = tokio::join!(conn.send(asked, text), async {
            match fake.asked().await {
                AgentCmd::Send {
                    request,
                    room_id,
                    text: sent,
                } => {
                    assert_eq!(room_id, expected, "{asked}");
                    // As it was given: the worker makes the message of it.
                    assert_eq!(sent, text);
                    fake.tell(MatrixEvent::Agent(AgentEvent::Sent {
                        request,
                        result: Ok(format!("$sent-in-{expected}")),
                    }));
                }
                other => panic!("not a send: {other:?}"),
            }
        });
        assert_eq!(sent.unwrap(), format!("$sent-in-{expected}"));
    }

    // A text that starts with a slash stays what it is (the worker sends it as text,
    // not as one of the app's commands); control characters do not travel.
    for (text, expected) in [
        ("/me waves", "/me waves"),
        ("/unknown thing", "/unknown thing"),
        ("a\u{1b}[2Jb\tc\nd\u{202e}", "a[2Jb\tc\nd"),
    ] {
        let (sent, ()) = tokio::join!(conn.send(ROOM, text), async {
            match fake.asked().await {
                AgentCmd::Send {
                    request,
                    text: sent,
                    ..
                } => {
                    assert_eq!(sent, expected);
                    fake.tell(MatrixEvent::Agent(AgentEvent::Sent {
                        request,
                        result: Ok("$e".into()),
                    }));
                }
                other => panic!("not a send: {other:?}"),
            }
        });
        sent.unwrap();
    }

    // What names no room of the account, or more than one, never reaches the worker.
    for (asked, code) in [
        ("Nowhere", Code::NotFound),
        ("!nowhere:example.org", Code::NotFound),
        ("#nolobby:example.org", Code::NotFound),
        // An invitation is not a room of the account.
        ("Party", Code::NotFound),
        ("!invite:example.org", Code::NotFound),
        ("agents", Code::NotFound),
        ("Twins", Code::Ambiguous),
        // The address of one room and the name of another: not guessed.
        ("#hall:example.org", Code::Ambiguous),
        // A room whose id cannot be handed out cannot be reached, by name or by id.
        ("Long", Code::NotFound),
        (long_id.as_str(), Code::NotFound),
        ("Unclean", Code::NotFound),
    ] {
        let refused = refusal(conn.send(asked, "hello").await);
        assert_eq!(refused.code, code, "{asked}");
        // The refusal names no room: not the twins, not their ids.
        assert!(!refused.message.contains("twin"), "{}", refused.message);
        assert!(!refused.message.contains('!'), "{}", refused.message);
    }
    assert_eq!(
        refusal(conn.send(ROOM, "  \n ").await).code,
        Code::BadRequest
    );
    assert!(fake.asked_nothing());

    // A failure of the Matrix side is told, cleaned.
    let (sent, ()) = tokio::join!(conn.send(ROOM, "hello"), async {
        match fake.asked().await {
            AgentCmd::Send { request, .. } => {
                fake.tell(MatrixEvent::Agent(AgentEvent::Sent {
                    request,
                    result: Err("M_FORBIDDEN: \u{1b}not allowed\nhere".into()),
                }));
            }
            other => panic!("not a send: {other:?}"),
        }
    });
    let refused = refusal(sent);
    assert_eq!(refused.code, Code::Failed);
    assert_eq!(refused.message, "M_FORBIDDEN: not allowed here");
    // An event id that cannot be handed out as it is, is not handed out changed.
    for bad in ["$with\nbreak".to_owned(), format!("${}", "e".repeat(300))] {
        let (sent, ()) = tokio::join!(conn.send(ROOM, "hello"), async {
            match fake.asked().await {
                AgentCmd::Send { request, .. } => {
                    fake.tell(MatrixEvent::Agent(AgentEvent::Sent {
                        request,
                        result: Ok(bad.clone()),
                    }));
                }
                other => panic!("not a send: {other:?}"),
            }
        });
        let refused = refusal(sent);
        assert_eq!(refused.code, Code::Failed);
        assert!(refused.message.contains("was sent"), "{}", refused.message);
    }
    fake.stop().await;
}

#[tokio::test]
async fn read_hands_out_the_messages_as_the_worker_tells_them() {
    let mut fake = Fake::ready("read", quick()).await;
    let mut conn = Conn::open(&fake.socket).await.unwrap();
    let messages = vec![
        message("$1", ANN, "one"),
        own("$2", "two"),
        Msg {
            kind: "undecryptable".into(),
            ..message("$3", EVE, "Unable to decrypt")
        },
        Msg {
            kind: "redacted".into(),
            ..message("$4", ANN, "Message deleted")
        },
    ];
    let (read, ()) = tokio::join!(conn.read(ROOM_NAME, 7), async {
        match fake.asked().await {
            AgentCmd::Read {
                request,
                room_id,
                limit,
            } => {
                assert_eq!((room_id.as_str(), limit), (ROOM, 7));
                fake.tell(MatrixEvent::Agent(AgentEvent::Read {
                    request,
                    result: Ok(messages.clone()),
                }));
            }
            other => panic!("not a read: {other:?}"),
        }
    });
    let (room_id, got) = read.unwrap();
    assert_eq!(room_id, ROOM);
    assert_eq!(got, messages);
    assert_eq!(got[1].sender, BOT);

    let (read, ()) = tokio::join!(conn.read(ROOM, 1), async {
        match fake.asked().await {
            AgentCmd::Read { request, .. } => {
                fake.tell(MatrixEvent::Agent(AgentEvent::Read {
                    request,
                    result: Err("could not open the room's messages".into()),
                }));
            }
            other => panic!("not a read: {other:?}"),
        }
    });
    assert_eq!(refusal(read).code, Code::Failed);
    // More than a read may ask for is refused before the worker hears of it.
    assert_eq!(refusal(conn.read(ROOM, 500).await).code, Code::BadRequest);
    assert_eq!(refusal(conn.read("Nowhere", 5).await).code, Code::NotFound);
    assert!(fake.asked_nothing());
    fake.stop().await;
}

#[tokio::test]
async fn listen_tells_what_is_new_or_changed_and_nothing_that_was_there_before() {
    let mut fake = Fake::start("listen", quick()).await;
    fake.tell(MatrixEvent::Rooms(vec![room(ROOM, ROOM_NAME)]));
    let socket = fake.socket.clone();
    let listening =
        tokio::spawn(async move { Conn::open(&socket).await.unwrap().listen(ROOM_NAME).await });
    match fake.asked().await {
        AgentCmd::Watch { room_id } => assert_eq!(room_id, ROOM),
        other => panic!("not a watch: {other:?}"),
    }
    // What the store held, and what the first sync brings (missed while `serve` was
    // not running), is not news: nobody listens before that sync is through.
    let old = message("$old", ANN, "before");
    let missed = message("$missed", ANN, "while away");
    fake.watched(vec![old.clone()]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!listening.is_finished());
    fake.watched(vec![old.clone(), missed.clone()]);
    fake.tell(MatrixEvent::CaughtUp);
    let mut listener = listening.await.unwrap().unwrap();
    assert_eq!(listener.room_id(), ROOM);

    // A new message is told once.
    let new = message("$new", ANN, "hello");
    let mut all = vec![old, missed, new.clone()];
    fake.watched(all.clone());
    assert_eq!(next(&mut listener).await.unwrap(), new);
    fake.watched(all.clone());
    // An edit is told again, under the same id (and nothing came in between).
    all[2] = Msg {
        body: "hello!".into(),
        edited: true,
        ..new
    };
    fake.watched(all.clone());
    assert_eq!(next(&mut listener).await.unwrap(), all[2]);
    // A message that cannot be decrypted is told as such, and again once it can.
    let locked = Msg {
        kind: "undecryptable".into(),
        ..message("$late", ANN, "Unable to decrypt")
    };
    all.push(locked.clone());
    fake.watched(all.clone());
    assert_eq!(next(&mut listener).await.unwrap(), locked);
    all[3] = message("$late", ANN, "now readable");
    fake.watched(all.clone());
    assert_eq!(next(&mut listener).await.unwrap(), all[3]);

    // A second listener is in at once (the room is watched already), and both hear.
    let mut second = Conn::open(&fake.socket)
        .await
        .unwrap()
        .listen(ROOM)
        .await
        .unwrap();
    assert!(fake.asked_nothing());
    all.push(own("$both", "to all"));
    fake.watched(all.clone());
    assert_eq!(next(&mut listener).await.unwrap().event_id, "$both");
    assert_eq!(next(&mut second).await.unwrap().event_id, "$both");
    // The timeline is emptied and filled again (as after a gap): nothing is told twice.
    fake.watched(Vec::new());
    fake.watched(all.clone());
    all.push(message("$after", EVE, "still here"));
    fake.watched(all.clone());
    assert_eq!(next(&mut listener).await.unwrap().event_id, "$after");
    assert_eq!(next(&mut second).await.unwrap().event_id, "$after");

    // When the last listener has gone, the room is no longer watched.
    drop(listener);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(fake.asked_nothing(), "one listener is still there");
    drop(second);
    match fake.asked().await {
        AgentCmd::Unwatch { room_id } => assert_eq!(room_id, ROOM),
        other => panic!("not an unwatch: {other:?}"),
    }
    fake.stop().await;
}

#[tokio::test]
async fn listening_before_the_first_sync_is_through_is_refused_after_a_while() {
    let limits = Limits {
        sync: Duration::from_millis(100),
        ..quick()
    };
    let mut fake = Fake::start("nosync", limits).await;
    fake.tell(MatrixEvent::Rooms(vec![room(ROOM, ROOM_NAME)]));
    let listening = Conn::open(&fake.socket).await.unwrap().listen(ROOM).await;
    assert_eq!(refusal(listening).code, Code::NotReady);
    // The room was asked for and is let go again.
    assert!(matches!(fake.asked().await, AgentCmd::Watch { .. }));
    assert!(matches!(fake.asked().await, AgentCmd::Unwatch { .. }));
    fake.stop().await;
}

#[tokio::test]
async fn a_listener_that_does_not_read_is_dropped_and_nothing_piles_up() {
    let limits = Limits {
        backlog: 4,
        write: Duration::from_millis(300),
        ..quick()
    };
    let mut fake = Fake::ready("slow", limits).await;
    let mut raw = Raw::open(&fake.socket).await;
    raw.say(&Request::Listen { room: ROOM.into() }.line()).await;
    assert!(matches!(fake.asked().await, AgentCmd::Watch { .. }));
    fake.watched(Vec::new());
    let header = raw.line().await;
    assert!(
        matches!(proto::parse_header(&header), Some(Ok(_))),
        "{header}"
    );

    // Far more than the socket and the backlog hold, and nobody reads.
    let body = "x".repeat(16 * 1024);
    const FLOOD: usize = 1500;
    for n in 0..FLOOD {
        fake.watched(vec![message(&format!("$flood{n}"), ANN, &body)]);
    }
    // The listener is dropped, and with it the room.
    assert!(matches!(fake.asked().await, AgentCmd::Unwatch { .. }));
    // What was written before that is all there is; then the connection ends.
    let mut heard = 0;
    loop {
        match raw.hear().await {
            Read::Line(line) => match proto::parse_item(&line) {
                Some(Item::Message(_)) => heard += 1,
                Some(Item::End(refusal)) => assert_eq!(refusal.code, Code::TooSlow),
                _ => panic!("neither a message nor the end: {} bytes", line.len()),
            },
            Read::Eof => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(
        heard < FLOOD / 10,
        "{heard} messages were kept for a listener that did not read"
    );
    // Others are served as before.
    assert_eq!(
        Conn::open(&fake.socket)
            .await
            .unwrap()
            .rooms()
            .await
            .unwrap()
            .len(),
        1
    );
    fake.stop().await;
}

#[tokio::test]
async fn one_connection_too_many_is_told_so_and_a_free_place_is_used_again() {
    let limits = Limits {
        connections: 2,
        ..quick()
    };
    let mut fake = Fake::ready("many", limits).await;
    let mut first = Conn::open(&fake.socket).await.unwrap();
    first.rooms().await.unwrap();
    let mut second = Conn::open(&fake.socket).await.unwrap();
    second.rooms().await.unwrap();
    for _ in 0..3 {
        let third = Conn::open(&fake.socket).await.unwrap().rooms().await;
        assert_eq!(refusal(third).code, Code::Busy);
    }
    // Those who are in go on.
    first.rooms().await.unwrap();
    second.rooms().await.unwrap();
    drop(second);
    // A place is free once `serve` has seen the other one go.
    let again = async {
        loop {
            match Conn::open(&fake.socket).await.unwrap().rooms().await {
                Ok(rooms) => return rooms,
                Err(ClientError::Refused(refusal)) if refusal.code == Code::Busy => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(other) => panic!("{other:?}"),
            }
        }
    };
    let rooms = tokio::time::timeout(Duration::from_secs(5), again)
        .await
        .expect("a free place is used again");
    assert_eq!(rooms.len(), 1);
    assert!(fake.asked_nothing());
    fake.stop().await;
}

#[tokio::test]
async fn what_is_not_a_request_is_answered_with_an_error_and_never_reaches_the_worker() {
    let mut fake = Fake::ready("junk", quick()).await;
    let mut raw = Raw::open(&fake.socket).await;
    for line in [
        "hello",
        "[1,2]",
        "{}",
        r#"{"op":"join","room":"!x:example.org"}"#,
        r#"{"op":"leave","room":"Agents"}"#,
        r#"{"op":"create","name":"mine"}"#,
        r#"{"op":"invite","room":"Agents","user":"@eve:example.org"}"#,
        r#"{"op":"accept","room":"!invite:example.org"}"#,
        r#"{"op":"redact","room":"Agents","event_id":"$1"}"#,
        r#"{"op":"shutdown"}"#,
        r#"{"op":"send","room":"Agents"}"#,
        r#"{"op":"read","room":"Agents","limit":1000}"#,
    ] {
        raw.say(line).await;
        assert_eq!(raw.refused().await.code, Code::BadRequest, "{line}");
    }
    // Not UTF-8: said, and the connection goes on.
    raw.writer.write_all(b"\xff\xfe{\n").await.unwrap();
    assert_eq!(raw.refused().await.code, Code::BadRequest);
    // An empty line is no request at all.
    raw.say("").await;
    raw.say(&Request::Rooms.line()).await;
    let header = proto::parse_header(&raw.line().await).unwrap().unwrap();
    assert_eq!(header["count"], 1);
    match proto::parse_item(&raw.line().await) {
        Some(Item::Room(room)) => assert_eq!(room.id, ROOM),
        _ => panic!("not a room"),
    }
    assert!(fake.asked_nothing());
    fake.stop().await;
}

#[tokio::test]
async fn a_line_that_is_too_long_is_refused_and_the_connection_closed() {
    let mut fake = Fake::ready("long", quick()).await;
    let Raw {
        mut lines,
        mut writer,
    } = Raw::open(&fake.socket).await;
    // Never a line end: `serve` must not wait for one, nor keep what comes.
    let writing = tokio::spawn(async move {
        let chunk = [b'x'; 8192];
        for _ in 0..64 {
            if writer.write_all(&chunk).await.is_err() {
                break;
            }
        }
        writer
    });
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next(None, None))
        .await
        .expect("an answer in time")
        .unwrap();
    let Read::Line(line) = line else {
        panic!("not a line: {line:?}");
    };
    match proto::parse_header(&line) {
        Some(Err(refusal)) => assert_eq!(refusal.code, Code::TooLong),
        _ => panic!("not a refusal: {line}"),
    }
    let end = tokio::time::timeout(Duration::from_secs(10), lines.next(None, None))
        .await
        .expect("the connection ends in time");
    assert!(matches!(end, Ok(Read::Eof) | Err(_)), "{end:?}");
    drop(writing.await);
    // A request of exactly the largest size is still read (and refused for what it
    // says, not for its length).
    let mut raw = Raw::open(&fake.socket).await;
    let frame = r#"{"op":"nothing","pad":""}"#.len();
    let padded = format!(
        r#"{{"op":"nothing","pad":"{}"}}"#,
        "p".repeat(proto::MAX_REQUEST_LINE - frame)
    );
    assert_eq!(padded.len(), proto::MAX_REQUEST_LINE);
    raw.say(&padded).await;
    assert_eq!(raw.refused().await.code, Code::BadRequest);
    assert!(fake.asked_nothing());
    fake.stop().await;
}

#[tokio::test]
async fn a_line_that_is_not_finished_and_a_silent_connection_are_closed() {
    let limits = Limits {
        line: Duration::from_millis(150),
        idle: Duration::from_millis(600),
        ..quick()
    };
    let mut fake = Fake::ready("half", limits).await;
    // Half a request, and then nothing.
    let mut raw = Raw::open(&fake.socket).await;
    raw.writer.write_all(br#"{"op":"roo"#).await.unwrap();
    let began = std::time::Instant::now();
    assert_eq!(raw.refused().await.code, Code::Timeout);
    assert_eq!(raw.hear().await, Read::Eof);
    assert!(began.elapsed() < Duration::from_secs(5));
    // Nothing at all: closed without a word.
    let mut raw = Raw::open(&fake.socket).await;
    let began = std::time::Instant::now();
    assert_eq!(raw.hear().await, Read::Eof);
    assert!(began.elapsed() >= Duration::from_millis(500));
    // A stream is not closed for being silent, but ends when the listener speaks.
    let mut raw = Raw::open(&fake.socket).await;
    raw.say(&Request::Listen { room: ROOM.into() }.line()).await;
    assert!(matches!(fake.asked().await, AgentCmd::Watch { .. }));
    fake.watched(Vec::new());
    assert!(matches!(
        proto::parse_header(&raw.line().await),
        Some(Ok(_))
    ));
    tokio::time::sleep(Duration::from_millis(900)).await;
    fake.watched(vec![message("$1", ANN, "still listening")]);
    assert!(matches!(
        proto::parse_item(&raw.line().await),
        Some(Item::Message(_))
    ));
    raw.say(&Request::Rooms.line()).await;
    assert_eq!(raw.hear().await, Read::Eof);
    fake.stop().await;
}

#[tokio::test]
async fn without_a_session_serve_does_not_start_and_tries_no_login() {
    for (name, event) in [
        ("nologin", Some(MatrixEvent::NeedLogin { reason: None })),
        (
            "blocked",
            Some(MatrixEvent::NeedLogin {
                reason: Some("The saved session file is damaged".into()),
            }),
        ),
        (
            "reauth",
            Some(MatrixEvent::NeedReauth {
                target: crate::matrix::ReauthTarget {
                    homeserver: "https://example.org".into(),
                    user_id: BOT.into(),
                    device_id: "DEVICE".into(),
                    kind: crate::matrix::ReauthKind::Password,
                    client_id: None,
                },
                reason: "expired".into(),
            }),
        ),
        ("gone", None),
    ] {
        let dir = Dir::new(name);
        let socket = dir.path().join("run").join("t.sock");
        let (matrix, mut commands) = MatrixHandle::for_test();
        let (events, received) = unbounded_channel();
        let expected = match event {
            Some(event) => {
                events.send(event).unwrap();
                End::NeedLogin
            }
            None => End::WorkerGone,
        };
        drop(events);
        let outcome = Served::begin(matrix, received, &socket, quick()).await;
        match outcome {
            Err(StartError::Session(end)) => assert_eq!(end, expected, "{name}"),
            Err(other) => panic!("{name}: {other:?}"),
            Ok(_) => panic!("{name}: serve started without a session"),
        }
        // The worker is told to stop, and nothing else: no login is ever tried.
        assert!(
            matches!(commands.try_recv(), Ok(MatrixCmd::Shutdown)),
            "{name}"
        );
        assert!(commands.try_recv().is_err(), "{name}");
        // No socket was made, nor its folder.
        assert!(!socket.parent().unwrap().exists(), "{name}");
    }
    let told = End::NeedLogin.message("bot");
    assert!(told.contains("matrix-rat --profile bot"), "{told}");
    assert!(told.contains("matrix-rat --profile bot serve"), "{told}");
}

#[tokio::test]
async fn a_socket_that_cannot_be_made_stops_the_worker() {
    let dir = Dir::new("nosock");
    // A file where the socket's folder should be.
    std::fs::write(dir.path().join("run"), "in the way").unwrap();
    let socket = dir.path().join("run").join("t.sock");
    let (matrix, mut commands) = MatrixHandle::for_test();
    let (events, received) = unbounded_channel();
    events
        .send(MatrixEvent::LoggedIn {
            user_id: BOT.into(),
        })
        .unwrap();
    let outcome = Served::begin(matrix, received, &socket, quick()).await;
    assert!(matches!(outcome, Err(StartError::Socket(_))));
    assert!(matches!(commands.try_recv(), Ok(MatrixCmd::Shutdown)));
}

#[tokio::test]
async fn when_the_session_ends_everyone_is_told_and_stopping_removes_the_socket() {
    let mut fake = Fake::ready("ends", quick()).await;
    let mut listener = listening(&mut fake, Vec::new()).await;
    let mut conn = Conn::open(&fake.socket).await.unwrap();
    let (sent, ()) = tokio::join!(conn.send(ROOM, "hello"), async {
        assert!(matches!(fake.asked().await, AgentCmd::Send { .. }));
        // The server no longer takes the session.
        fake.tell(MatrixEvent::NeedLogin {
            reason: Some("Your session has expired".into()),
        });
    });
    assert_eq!(refusal(sent).code, Code::Stopping);
    // The listener is told that this is the end, in so many words.
    match next(&mut listener).await {
        Err(ClientError::Refused(refusal)) => assert_eq!(refusal.code, Code::Stopping),
        other => panic!("{other:?}"),
    }
    let mut served = fake.served.take().unwrap();
    let end = tokio::time::timeout(Duration::from_secs(5), served.ended())
        .await
        .expect("serve notices the end of the session");
    assert_eq!(end, End::NeedLogin);
    // Asked again, it says the same.
    assert_eq!(served.ended().await, End::NeedLogin);
    assert!(fake.socket.exists());
    served.stop().await;
    assert!(
        !fake.socket.exists(),
        "the socket is removed on the way out"
    );
    assert!(matches!(
        Conn::open(&fake.socket).await,
        Err(ClientError::NotRunning)
    ));
    // The worker is told to stop, as the app tells it on quitting; it was never
    // asked to log in again.
    let mut last = None;
    while let Ok(command) = fake.commands.try_recv() {
        assert!(
            matches!(command, MatrixCmd::Agent(_) | MatrixCmd::Shutdown),
            "{}",
            command.describe()
        );
        last = Some(command);
    }
    assert!(matches!(last, Some(MatrixCmd::Shutdown)));
}

#[tokio::test]
async fn stopping_closes_the_connections_and_removes_the_socket() {
    let mut fake = Fake::ready("stop", quick()).await;
    let mut conn = Conn::open(&fake.socket).await.unwrap();
    conn.rooms().await.unwrap();
    let socket = fake.socket.clone();
    assert!(socket.exists());
    fake.stop().await;
    assert!(!socket.exists());
    assert!(conn.rooms().await.is_err());
    assert!(matches!(
        Conn::open(&socket).await,
        Err(ClientError::NotRunning)
    ));
    assert!(matches!(fake.commands.try_recv(), Ok(MatrixCmd::Shutdown)));
    assert!(fake.commands.try_recv().is_err());
}

#[tokio::test]
async fn a_session_that_starts_again_is_watched_again_and_tells_only_what_is_new() {
    let mut fake = Fake::ready("again", quick()).await;
    let old = message("$old", ANN, "before");
    let mut listener = listening(&mut fake, vec![old.clone()]).await;
    let first = message("$first", ANN, "one");
    fake.watched(vec![old.clone(), first.clone()]);
    assert_eq!(next(&mut listener).await.unwrap(), first);

    // A send is on its way when the worker starts its session again (after an error).
    let mut conn = Conn::open(&fake.socket).await.unwrap();
    let (sent, ()) = tokio::join!(conn.send(ROOM, "hello"), async {
        assert!(matches!(fake.asked().await, AgentCmd::Send { .. }));
        fake.tell(MatrixEvent::LoggedIn {
            user_id: BOT.into(),
        });
    });
    let refused = refusal(sent);
    assert_eq!(refused.code, Code::Failed);
    assert!(
        refused.message.contains("may or may not"),
        "{}",
        refused.message
    );
    // The room is asked for again, and what was told before is not told again.
    match fake.asked().await {
        AgentCmd::Watch { room_id } => assert_eq!(room_id, ROOM),
        other => panic!("not a watch: {other:?}"),
    }
    let second = message("$second", ANN, "two");
    fake.watched(vec![old, first, second.clone()]);
    assert_eq!(next(&mut listener).await.unwrap(), second);
    fake.stop().await;
}

#[tokio::test]
async fn a_watched_room_that_is_left_or_cannot_be_followed_ends_its_streams() {
    let mut fake = Fake::ready("left", quick()).await;
    // The worker cannot follow the room.
    let socket = fake.socket.clone();
    let asking = tokio::spawn(async move { Conn::open(&socket).await.unwrap().listen(ROOM).await });
    assert!(matches!(fake.asked().await, AgentCmd::Watch { .. }));
    fake.tell(MatrixEvent::Agent(AgentEvent::WatchEnded {
        room_id: ROOM.into(),
        reason: "could not open\u{7} the room's messages".into(),
    }));
    let refused = refusal(asking.await.unwrap());
    assert_eq!(refused.code, Code::Failed);
    assert_eq!(refused.message, "could not open the room's messages");

    // The account is taken out of the room while someone listens.
    let mut listener = listening(&mut fake, Vec::new()).await;
    fake.tell(MatrixEvent::Rooms(Vec::new()));
    match next(&mut listener).await {
        Err(ClientError::Refused(refusal)) => assert_eq!(refusal.code, Code::NotFound),
        other => panic!("{other:?}"),
    }
    assert!(matches!(fake.asked().await, AgentCmd::Unwatch { .. }));
    // And it cannot be listened to any more.
    let again = Conn::open(&fake.socket).await.unwrap().listen(ROOM).await;
    assert_eq!(refusal(again).code, Code::NotFound);
    fake.stop().await;
}

/// Stopping (a signal) is told to everyone who is connected before the connections
/// are closed: nobody is left with a connection that just ends.
#[tokio::test]
async fn stopping_tells_those_who_wait_and_those_who_listen() {
    let mut fake = Fake::ready("told", quick()).await;
    let mut listener = listening(&mut fake, Vec::new()).await;
    let mut asking = Conn::open(&fake.socket).await.unwrap();
    let mut idle = Conn::open(&fake.socket).await.unwrap();
    idle.rooms().await.unwrap();
    let socket = fake.socket.clone();
    let began = std::time::Instant::now();
    let (sent, ()) = tokio::join!(asking.send(ROOM, "hello"), async {
        // The worker has the send and never answers it; then `serve` is stopped.
        assert!(matches!(fake.asked().await, AgentCmd::Send { .. }));
        fake.stop().await;
    });
    assert_eq!(refusal(sent).code, Code::Stopping);
    match next(&mut listener).await {
        Err(ClientError::Refused(refusal)) => assert_eq!(refusal.code, Code::Stopping),
        other => panic!("{other:?}"),
    }
    // Nobody was waited for longer than it took to tell them.
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    // A connection on which nothing was being asked is closed without a word.
    assert!(idle.rooms().await.is_err());
    assert!(!socket.exists());
    assert!(matches!(
        Conn::open(&socket).await,
        Err(ClientError::NotRunning)
    ));
}

/// A `Served` that is dropped without being stopped leaves nothing running behind a
/// socket whose file is gone.
#[tokio::test]
async fn a_serve_that_is_dropped_leaves_nothing_running() {
    let mut fake = Fake::ready("dropped", quick()).await;
    let mut conn = Conn::open(&fake.socket).await.unwrap();
    conn.rooms().await.unwrap();
    let mut listener = listening(&mut fake, Vec::new()).await;
    drop(fake.served.take());
    assert!(!fake.socket.exists());
    // The connections are cut off...
    assert!(conn.rooms().await.is_err());
    assert!(next(&mut listener).await.is_err());
    // ...and the hub is gone: with it the last handle to the worker, which is how a
    // worker learns that it has to end.
    let gone = async {
        while let Some(command) = fake.commands.recv().await {
            assert!(
                matches!(command, MatrixCmd::Agent(_)),
                "{}",
                command.describe()
            );
        }
    };
    tokio::time::timeout(Duration::from_secs(5), gone)
        .await
        .expect("the hub lets go of the worker");
}

/// A message is told again when its sender is trusted differently: what a client
/// did not take as the user's word it may take now, and the other way round.
#[tokio::test]
async fn a_message_whose_sender_becomes_verified_is_told_again() {
    use crate::agent::testing::doubted;
    let mut fake = Fake::ready("trust", quick()).await;
    let unverified = doubted("$1", ANN, "yes", None, "unverified_identity");
    let mut listener = listening(&mut fake, Vec::new()).await;
    fake.watched(vec![unverified.clone()]);
    assert_eq!(next(&mut listener).await.unwrap(), unverified);
    let verified = message("$1", ANN, "yes");
    fake.watched(vec![verified.clone()]);
    let told = next(&mut listener).await.unwrap();
    assert_eq!(told, verified);
    assert!(told.verified && told.untrusted.is_none());
    fake.watched(vec![unverified.clone()]);
    assert_eq!(next(&mut listener).await.unwrap(), unverified);
    fake.stop().await;
}

/// Whatever the worker tells that is meant for a screen, `serve` does nothing about
/// it: above all it accepts no invitation and answers no verification.
#[tokio::test]
async fn what_is_meant_for_a_screen_makes_serve_ask_for_nothing() {
    use crate::matrix::{RecoveryStatus, SyncStatus};
    let mut fake = Fake::ready("screen", quick()).await;
    let mut invite = room("!invite:example.org", "Come in");
    invite.category = RoomCategory::Invite;
    for event in [
        MatrixEvent::Rooms(vec![room(ROOM, ROOM_NAME), invite]),
        MatrixEvent::Sync(SyncStatus::Offline),
        MatrixEvent::Sync(SyncStatus::Error("no route".into())),
        MatrixEvent::Sync(SyncStatus::Synced),
        MatrixEvent::Error("the session failed".into()),
        MatrixEvent::ActionFailed("something failed".into()),
        MatrixEvent::Recovery(RecoveryStatus::NeedsKey),
        MatrixEvent::Spaces(Vec::new()),
        MatrixEvent::IgnoredUsers(vec![EVE.into()]),
        MatrixEvent::UploadLimit(1),
        MatrixEvent::SuppressedNotice {
            room_id: ROOM.into(),
        },
        MatrixEvent::NoticeClicked {
            room_id: "!invite:example.org".into(),
        },
        MatrixEvent::Draft {
            room_id: ROOM.into(),
            thread: None,
            text: "a draft".into(),
        },
        MatrixEvent::BrowserLoginUrl("https://example.org/login".into()),
        MatrixEvent::LoginFailed("no".into()),
    ] {
        fake.tell(event);
    }
    // All of it is worked in before the next request is answered.
    let rooms = Conn::open(&fake.socket)
        .await
        .unwrap()
        .rooms()
        .await
        .unwrap();
    assert_eq!(rooms.len(), 1);
    assert!(fake.asked_nothing());
    fake.stop().await;
}

/// The net for a field added later: the hub, filled with recognisable private things,
/// says none of them in `Debug`; nor does what travels with it.
#[test]
fn the_debug_output_of_the_hub_holds_nothing_private() {
    const PRIVATE: &[&str] = &[
        "hidden-room",
        "Secret Garden",
        "#garden",
        "@alice",
        "@self-secret",
        "$launch",
        "launch codes",
        "needle",
        "example.org",
    ];
    let (matrix, _commands) = MatrixHandle::for_test();
    let (_events, received) = unbounded_channel();
    let (_closing, closed) = watch::channel(false);
    let (mut hub, _handle) = Hub::new(matrix, received, quick(), closed);
    let mut held = Vec::new();
    // A request that waits for the rooms.
    let (reply, answer) = oneshot::channel();
    hub.on_request(HubRequest::Send {
        room: "Secret Garden".into(),
        text: "the launch codes".into(),
        reply,
    });
    held.push(format!("{hub:?}"));
    assert!(
        hub.on_event(MatrixEvent::LoggedIn {
            user_id: "@self-secret:example.org".into(),
        })
        .is_none()
    );
    let mut garden = room("!hidden-room:example.org", "Secret Garden");
    garden.alias = Some("#garden:example.org".into());
    hub.on_event(MatrixEvent::Rooms(vec![garden]));
    hub.on_event(MatrixEvent::CaughtUp);
    let (reply, _reading) = oneshot::channel();
    hub.on_request(HubRequest::Read {
        room: "#garden:example.org".into(),
        limit: 5,
        reply,
    });
    let (reply, mut asked) = oneshot::channel();
    hub.on_request(HubRequest::Listen {
        room: "!hidden-room:example.org".into(),
        reply,
    });
    let secret = message(
        "$launch:example.org",
        "@alice:example.org",
        "the needle in the haystack",
    );
    hub.on_event(MatrixEvent::Agent(AgentEvent::Watched {
        room_id: "!hidden-room:example.org".into(),
        messages: vec![secret.clone()],
    }));
    let listener = asked.try_recv().expect("listening").expect("not refused");
    hub.on_event(MatrixEvent::Agent(AgentEvent::Watched {
        room_id: "!hidden-room:example.org".into(),
        messages: vec![
            secret,
            message("$launch2:example.org", "@alice:example.org", "launch codes"),
        ],
    }));
    held.push(format!("{hub:?}"));
    let shown = held.join("\n");
    for count in [
        "rooms: Some(1)",
        "waiting: 1",
        "sends: 1",
        "reads: 1",
        "watched: 1",
        "listeners: 1",
    ] {
        assert!(shown.contains(count), "{count} not in {shown}");
    }
    // What travels: a refusal with the Matrix side's own words, the ends, the limits.
    let shown = format!(
        "{shown}\n{:?} {:?} {:?} {:?}",
        Refusal::failed("Could not send to #garden:example.org"),
        End::NeedLogin,
        StartError::Session(End::WorkerGone),
        quick(),
    );
    for private in PRIVATE {
        assert!(!shown.contains(private), "{private} in {shown}");
    }
    assert_eq!(listener.room_id, "!hidden-room:example.org");
    drop(answer);
}
