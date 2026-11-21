//! What the interface holds for the rooms it has shown, and what one change of the open
//! room and one frame cost it (see `crate::measure` for how to run these). The
//! messages are the worker's own for a sample room (`crate::matrix::sample_messages`),
//! so they are as big as real ones of that mix.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::App;
use crate::matrix::MatrixEvent;
use crate::measure::{Times, grew, heap_in_use, mb, time_runs};
use crate::model::{Message, Room, RoomKind};

/// Runs of each timed step.
const RUNS: usize = 9;

/// What glibc takes for a block of `len` bytes: 8 bytes of header, in steps of 16,
/// 32 at the least.
fn chunk(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        ((len + 8).div_ceil(16) * 16).max(32)
    }
}

fn text(s: &str) -> usize {
    s.len()
}

fn media(source: &matrix_sdk::ruma::events::room::MediaSource) -> usize {
    use matrix_sdk::ruma::events::room::MediaSource;
    match source {
        MediaSource::Plain(uri) => chunk(text(uri.as_str())),
        MediaSource::Encrypted(file) => {
            chunk(std::mem::size_of_val(&**file)) + chunk(text(file.url.as_str())) + 5 * 48
        }
    }
}

/// The heap blocks of one message, by part, as glibc rounds them. An estimate made
/// from the lengths (`String`s made by `to_owned` and `format!` hold about what they
/// say); the allocator's own count is measured separately.
fn parts(m: &Message) -> [(&'static str, usize); 12] {
    use crate::model::{ImageSource, ItemRef};
    let s = |s: &String| chunk(s.capacity());
    let item = match &m.item {
        Some(ItemRef::Event(id) | ItemRef::Transaction(id)) => s(id),
        None => 0,
    };
    let reply = m
        .reply_to
        .as_ref()
        .map_or(0, |(sender, quoted)| s(sender) + s(quoted))
        + m.reply_event.as_ref().map_or(0, s);
    let reactions = chunk(m.reactions.capacity() * std::mem::size_of::<crate::model::Reaction>())
        + m.reactions.iter().map(|r| s(&r.key)).sum::<usize>();
    let image = m.image.as_ref().map_or(0, |image| {
        let source = match &image.source {
            ImageSource::Matrix(media_ref) => {
                chunk(std::mem::size_of::<crate::model::MediaRef>())
                    + media(&media_ref.full)
                    + media_ref.thumbnail.as_ref().map_or(0, |(t, _)| media(t))
            }
            _ => 0,
        };
        s(&image.key) + s(&image.name) + image.caption.as_ref().map_or(0, s) + source
    });
    let file = m.file.as_ref().map_or(0, |file| {
        s(&file.key)
            + s(&file.name)
            + chunk(std::mem::size_of_val(&*file.source))
            + media(&file.source)
    });
    let read_by = chunk(m.read_by.readers.capacity() * std::mem::size_of::<crate::model::Reader>())
        + m.read_by
            .readers
            .iter()
            .map(|r| s(&r.name) + s(&r.time))
            .sum::<usize>();
    let mentions = m.mentions.as_ref().map_or(0, |mentions| {
        // A B-tree node holds up to 11 ids; ids are boxed strings.
        let ids = mentions.user_ids.len();
        (if ids > 0 { chunk(11 * 16 + 8) } else { 0 })
            + mentions
                .user_ids
                .iter()
                .map(|id| chunk(id.as_str().len()))
                .sum::<usize>()
    });
    let other = m.send_error.as_ref().map_or(0, s)
        + m.thread.as_ref().map_or(0, |thread| {
            thread
                .latest
                .as_ref()
                .map_or(0, |(sender, text)| s(sender) + s(text))
        })
        + m.poll.as_ref().map_or(0, |poll| {
            s(&poll.question)
                + chunk(poll.answers.capacity() * std::mem::size_of::<crate::model::PollAnswer>())
                + poll
                    .answers
                    .iter()
                    .map(|a| s(&a.id) + s(&a.text))
                    .sum::<usize>()
        });
    [
        ("body", s(&m.body)),
        ("html", m.html.as_ref().map_or(0, s)),
        ("id + event id", s(&m.id) + item),
        (
            "sender name + id",
            s(&m.sender) + m.sender_id.as_ref().map_or(0, s),
        ),
        ("time + day", s(&m.time) + s(&m.day)),
        ("reply", reply),
        ("reactions", reactions),
        ("picture", image),
        ("file", file),
        ("read by", read_by),
        ("mentions", mentions),
        ("other", other),
    ]
}

/// An app as the account of the brief has it: `rooms` rooms, pictures off, the
/// default settings for what is drawn, the first room open.
fn app_with_rooms(rooms: usize) -> App {
    let mut app = App::demo();
    let config = crate::config::Config::default();
    app.images = crate::images::ImageStore::new(crate::images::Graphics::off());
    app.read_markers = config.read_markers;
    app.link_previews = config.link_previews;
    app.room_previews = config.room_previews;
    app.rooms = (0..rooms)
        .map(|n| {
            Room::new(
                &format!("!room{n}:example.org"),
                &format!("Room number {n}"),
                RoomKind::Room,
            )
        })
        .collect();
    app.open_room(0);
    app
}

/// The worker's news that the open room's messages changed.
fn change(app: &App, messages: Vec<Message>) -> MatrixEvent {
    MatrixEvent::Timeline {
        room_id: app.rooms[app.active].id.clone(),
        generation: app.generation,
        messages,
        reached_start: false,
        detached: None,
    }
}

/// The messages that are events (the worker's read marker may be left out).
fn events(messages: &[Message]) -> usize {
    messages.iter().filter(|m| m.item.is_some()).count()
}

fn draw(app: &mut App, terminal: &mut Terminal<TestBackend>) {
    terminal.draw(|f| crate::ui::draw(f, app)).unwrap();
}

/// What one message holds, for the sample mix: inline and on the heap, by part.
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_bytes_per_message() {
    if !crate::measure::wanted() {
        return;
    }
    let messages = crate::matrix::sample_messages(5000).await;
    let n = messages.len();
    let before = heap_in_use();
    let copy = messages.clone();
    let after = heap_in_use();
    let measured = grew(before, after) as f64 / n as f64;
    println!(
        "[message] size_of::<Message>() = {} B, size_of::<Room>() = {} B",
        std::mem::size_of::<Message>(),
        std::mem::size_of::<Room>()
    );
    println!(
        "[message] {n} messages of the mix: {:.0} B each, as the allocator counts a copy \
         (inline part included)",
        measured
    );
    // By part, for the copy (as tight as a clone makes it) and for the worker's own
    // messages as it sends them (with what its strings and lists have to spare).
    let total = |messages: &[Message]| {
        let mut totals: Vec<(&str, usize)> =
            parts(&messages[0]).iter().map(|(k, _)| (*k, 0)).collect();
        for message in messages {
            for (total, (_, bytes)) in totals.iter_mut().zip(parts(message)) {
                total.1 += bytes;
            }
        }
        totals
    };
    let (tight, sent) = (total(&copy), total(&messages));
    let heap = |totals: &[(&str, usize)]| totals.iter().map(|(_, b)| b).sum::<usize>();
    println!(
        "[message] estimated heap blocks: {:.0} B/message in a copy, {:.0} B/message as the \
         worker sends them (and the interface keeps them); by part:",
        heap(&tight) as f64 / n as f64,
        heap(&sent) as f64 / n as f64
    );
    for ((part, bytes), (_, sent)) in tight.iter().zip(&sent) {
        println!(
            "    {part:<18} {:>7.1} B/message  {:>5.1}%   as sent: {:>7.1} B/message",
            *bytes as f64 / n as f64,
            100.0 * *bytes as f64 / heap(&tight).max(1) as f64,
            *sent as f64 / n as f64
        );
    }
    // The inline part, by field: every message carries all of it, used or not.
    let m = Message::default();
    let mut fields = [
        ("id", std::mem::size_of_val(&m.id)),
        ("item", std::mem::size_of_val(&m.item)),
        (
            "sender + sender_id",
            std::mem::size_of_val(&m.sender) + std::mem::size_of_val(&m.sender_id),
        ),
        (
            "time + day + sent_at",
            std::mem::size_of_val(&m.time)
                + std::mem::size_of_val(&m.day)
                + std::mem::size_of_val(&m.sent_at),
        ),
        (
            "body + html",
            std::mem::size_of_val(&m.body) + std::mem::size_of_val(&m.html),
        ),
        (
            "reply_to + reply_event",
            std::mem::size_of_val(&m.reply_to) + std::mem::size_of_val(&m.reply_event),
        ),
        ("reactions", std::mem::size_of_val(&m.reactions)),
        ("image", std::mem::size_of_val(&m.image)),
        ("file", std::mem::size_of_val(&m.file)),
        ("thread", std::mem::size_of_val(&m.thread)),
        ("poll", std::mem::size_of_val(&m.poll)),
        (
            "upload + send_error",
            std::mem::size_of_val(&m.upload) + std::mem::size_of_val(&m.send_error),
        ),
        (
            "shield + utd",
            std::mem::size_of_val(&m.shield) + std::mem::size_of_val(&m.utd),
        ),
        ("mentions", std::mem::size_of_val(&m.mentions)),
        ("read_by", std::mem::size_of_val(&m.read_by)),
    ];
    fields.sort_by_key(|(_, size)| std::cmp::Reverse(*size));
    println!(
        "[message] inline, by field (of {} B): {}",
        std::mem::size_of::<Message>(),
        fields
            .iter()
            .map(|(field, size)| format!("{field} {size}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let with = |f: &dyn Fn(&Message) -> bool| copy.iter().filter(|m| f(m)).count();
    println!(
        "[message] mix: {} with html, {} replies, {} with reactions, {} pictures, {} files, \
         {} with readers, {} own",
        with(&|m: &Message| m.html.is_some()),
        with(&|m: &Message| m.reply_to.is_some()),
        with(&|m: &Message| !m.reactions.is_empty()),
        with(&|m: &Message| m.image.is_some()),
        with(&|m: &Message| m.file.is_some()),
        with(&|m: &Message| !m.read_by.is_empty()),
        with(&|m: &Message| m.own),
    );
}

/// What the interface keeps of the rooms it has shown: every room's last messages stay
/// in `App.rooms[].messages` after it is left, for as long as the room is in the list.
/// The account of the brief: 150 rooms, 5 opened, 2000 messages each.
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_rooms_kept_by_the_interface() {
    if !crate::measure::wanted() {
        return;
    }
    const OPENED: usize = 5;
    let messages = crate::matrix::sample_messages(2000).await;
    let mut terminal = Terminal::new(TestBackend::new(200, 60)).unwrap();
    let before = heap_in_use();
    let mut app = app_with_rooms(150);
    draw(&mut app, &mut terminal);
    let empty = heap_in_use();
    for room in 0..OPENED {
        app.open_room(room);
        app.on_matrix(change(&app, messages.clone()));
        draw(&mut app, &mut terminal);
    }
    let after = heap_in_use();
    let kept: Vec<usize> = app.rooms.iter().map(|r| r.messages.len()).collect();
    assert!(
        kept[..OPENED].iter().all(|&n| events(&messages) <= n),
        "{kept:?}"
    );
    assert!(kept[OPENED..].iter().all(|&n| n == 0));
    println!(
        "[rooms] app with 150 empty rooms: {:+.2} MB",
        mb(grew(before, empty) as f64)
    );
    println!(
        "[rooms] after opening {OPENED} rooms of {} messages one after the other: {:+.1} MB; \
         all {OPENED} keep their messages (only the open one is shown)",
        messages.len(),
        mb(grew(empty, after) as f64)
    );
    let rows = app
        .row_cache
        .as_ref()
        .map(|c| format!("{c:?}"))
        .unwrap_or_default();
    println!("[rooms] row cache of the open room: {rows}");
}

/// One change of an open room of 5000 messages in the interface (the worker's news
/// taken in: the messages replaced, flags and search brought up to date), and the
/// frames after it on a 200x60 screen without pictures: the first lays out every
/// message again (the rows are cached per change), the next ones reuse the rows.
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_interface_cost_of_a_change_and_a_frame() {
    if !crate::measure::wanted() {
        return;
    }
    let messages = crate::matrix::sample_messages(5000).await;
    let mut terminal = Terminal::new(TestBackend::new(200, 60)).unwrap();
    let mut app = app_with_rooms(150);
    app.on_matrix(change(&app, messages.clone()));
    draw(&mut app, &mut terminal);
    assert!(events(&messages) <= app.rooms[0].messages.len());

    // The change alone; the copy it brings is made before the clock starts.
    let mut changes: Vec<MatrixEvent> = (0..RUNS).map(|_| change(&app, messages.clone())).collect();
    let take = time_runs(RUNS, || app.on_matrix(changes.pop().unwrap()));
    println!("[per change] the interface takes in 5000 messages: {take}");

    // A frame after a change: every row is laid out again.
    let mut miss = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        app.on_matrix(change(&app, messages.clone()));
        let start = std::time::Instant::now();
        draw(&mut app, &mut terminal);
        miss.push(start.elapsed());
    }
    miss.sort();
    println!(
        "[per frame] first frame after a change (layout of all rows): {}",
        Times(miss)
    );

    // Frames with nothing changed: the rows are reused.
    let hit = time_runs(RUNS, || draw(&mut app, &mut terminal));
    println!("[per frame] a frame with nothing changed (rows reused): {hit}");

    // Scrolled up a page: still the cached rows.
    app.timeline.scroll = 200;
    let scrolled = time_runs(RUNS, || draw(&mut app, &mut terminal));
    println!("[per frame] a frame scrolled up 200 rows (rows reused): {scrolled}");
    app.timeline.scroll = 0;

    // What the cached rows hold.
    app.row_cache = None;
    let before = heap_in_use();
    draw(&mut app, &mut terminal);
    let after = heap_in_use();
    println!(
        "[rows] {:?}: {:+.1} MB ({:.0} B per message)",
        app.row_cache.as_ref().unwrap(),
        mb(grew(before, after) as f64),
        grew(before, after) as f64 / messages.len() as f64
    );

    // A frame after a change of one message: a reaction comes to one in the middle, and
    // goes again with the next change.
    let mut one = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        let mut changed = messages.clone();
        if run % 2 == 0 {
            let middle = changed.len() / 2;
            changed[middle].reactions.push(crate::model::Reaction {
                key: "👍".to_owned(),
                count: 1,
                own: false,
            });
        }
        app.on_matrix(change(&app, changed));
        let start = std::time::Instant::now();
        draw(&mut app, &mut terminal);
        one.push(start.elapsed());
    }
    one.sort();
    println!(
        "[per frame] first frame after a change of one message (a reaction): {}",
        Times(one)
    );

    // A frame after a new message at the end.
    let mut grown = messages.clone();
    let mut new = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        let day = grown.last().map(|m| m.day.clone()).unwrap_or_default();
        grown.push(Message {
            id: format!("$new-{run}"),
            item: Some(crate::model::ItemRef::Event(format!("$new-{run}"))),
            sender: "Somebody new".to_owned(),
            sender_id: Some("@new:example.org".to_owned()),
            time: "23:59".to_owned(),
            day,
            body: format!("One more message, number {run}, at the end of the room."),
            ..Message::default()
        });
        app.on_matrix(change(&app, grown.clone()));
        let start = std::time::Instant::now();
        draw(&mut app, &mut terminal);
        new.push(start.elapsed());
    }
    new.sort();
    println!(
        "[per frame] first frame after a new message at the end: {}",
        Times(new)
    );

    // A frame at another width: every message is laid out again.
    let mut resized = Vec::with_capacity(RUNS);
    for run in 0..RUNS {
        terminal
            .backend_mut()
            .resize(if run % 2 == 0 { 199 } else { 200 }, 60);
        let start = std::time::Instant::now();
        draw(&mut app, &mut terminal);
        resized.push(start.elapsed());
    }
    resized.sort();
    println!(
        "[per frame] first frame at another width (layout of all rows): {}",
        Times(resized)
    );
}

/// The worker's news of a busy open room while the external editor has the terminal:
/// what `CHANGES` changes of a room of 5000 messages hold while they wait. Left in the
/// worker's channel (as they were before), each change is a whole copy of the messages;
/// held as the run loop holds them now, the open timeline is kept once.
#[tokio::test]
#[ignore = "a measurement: run alone and in release, see crate::measure"]
async fn measure_news_waiting_while_the_editor_runs() {
    use super::external_editor::{HeldEvents, hold_events_while};
    const CHANGES: usize = 20;
    if !crate::measure::wanted() {
        return;
    }
    let messages = crate::matrix::sample_messages(5000).await;
    let change = |messages: Vec<Message>| MatrixEvent::Timeline {
        room_id: "!open:example.org".to_owned(),
        generation: 1,
        messages,
        reached_start: false,
        detached: None,
    };
    let (worker, mut events) = tokio::sync::mpsc::unbounded_channel();

    // In the channel, one copy per change.
    let before = heap_in_use();
    for _ in 0..CHANGES {
        worker.send(change(messages.clone())).unwrap();
    }
    let queued = heap_in_use();
    while events.try_recv().is_ok() {}
    println!(
        "[editor] {CHANGES} changes of {} messages waiting in the channel: {:+.1} MB",
        messages.len(),
        mb(grew(before, queued) as f64)
    );

    // Held while the editor runs: the editor here is what sends the changes.
    let mut held = HeldEvents::default();
    let before = heap_in_use();
    let editor = async {
        for _ in 0..CHANGES {
            worker.send(change(messages.clone())).unwrap();
            tokio::task::yield_now().await;
        }
    };
    hold_events_while(editor, Some(&mut events), &mut held).await;
    let kept = heap_in_use();
    let mut count = 0;
    while held.pop().is_some() {
        count += 1;
    }
    assert_eq!(count, 1);
    println!(
        "[editor] the same {CHANGES} changes held while the editor runs: {:+.1} MB ({count} kept)",
        mb(grew(before, kept) as f64)
    );
}
