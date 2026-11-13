//! Who has read up to where: the read receipts the SDK attaches to timeline items,
//! turned into names on the messages the UI shows.
//!
//! Only what other people published is used. Our own receipt and the sender's own
//! (everyone has "read" what they wrote) are left out. A room's timeline carries the
//! receipts of the room, a thread's those of the thread.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Local, TimeZone};
use matrix_sdk::Room;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedUserId, UserId};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{EventTimelineItem, TimelineItem};

use super::convert::{day_label, display_name};
use crate::model::{Message, ReadBy, Reader};
use crate::text::{sanitize_line, truncate};

/// Names looked up in the member list are remembered up to this many.
const NAMES_MAX: usize = 2000;
/// Longest name shown, in columns.
const NAME_WIDTH: usize = 40;

type Stamp = Option<MilliSecondsSinceUnixEpoch>;

/// Display names of readers who have not written anything that is loaded (those who
/// have are named by their messages). Lives as long as the room stays open.
#[derive(Default)]
pub struct Names {
    known: HashMap<OwnedUserId, String>,
}

impl Names {
    /// Looks up the names that `annotate` is going to need and does not have yet.
    pub async fn learn(&mut self, room: &Room, items: &Vector<Arc<TimelineItem>>, own: &UserId) {
        let speakers = speakers(items);
        let mut wanted: Vec<OwnedUserId> = Vec::new();
        for event in items.iter().filter_map(|item| item.as_event()) {
            for (user, _) in pick(receipts(event), own, event.sender()).0 {
                if !speakers.contains_key(user)
                    && !self.known.contains_key(user)
                    && !wanted.iter().any(|known| known == user)
                {
                    wanted.push(user.to_owned());
                }
            }
        }
        if wanted.is_empty() {
            return;
        }
        if self.known.len() + wanted.len() > NAMES_MAX {
            self.known.clear();
        }
        for user in wanted.into_iter().take(NAMES_MAX) {
            // From the local store only: a receipt is not worth a request.
            let name = match room.get_member_no_sync(&user).await {
                Ok(Some(member)) => member.display_name().map(str::to_owned),
                _ => None,
            };
            let name = clean_name(name.as_deref(), &user);
            self.known.insert(user, name);
        }
    }

    /// Fills in `read_by` on the messages made from `items` (matched by their id).
    pub fn annotate(
        &self,
        messages: &mut [Message],
        items: &Vector<Arc<TimelineItem>>,
        own: &UserId,
    ) {
        let now = Local::now();
        let speakers = speakers(items);
        let mut read: HashMap<&str, ReadBy> = HashMap::new();
        for item in items {
            let Some(event) = item.as_event() else {
                continue;
            };
            let (picked, total) = pick(receipts(event), own, event.sender());
            if total == 0 {
                continue;
            }
            let readers = picked
                .into_iter()
                .map(|(user, stamp)| Reader {
                    name: match speakers.get(user).or_else(|| self.known.get(user)) {
                        Some(name) => name.clone(),
                        None => clean_name(None, user),
                    },
                    time: stamp
                        .and_then(local_time)
                        .map(|when| time_label(&when, &now))
                        .unwrap_or_default(),
                })
                .collect();
            read.insert(item.unique_id().0.as_str(), ReadBy { readers, total });
        }
        for message in messages {
            message.read_by = read.remove(message.id.as_str()).unwrap_or_default();
        }
    }
}

/// The names people go by in what they wrote, which is the freshest we have.
fn speakers(items: &Vector<Arc<TimelineItem>>) -> HashMap<&UserId, String> {
    items
        .iter()
        .filter_map(|item| item.as_event())
        .map(|event| {
            let name = display_name(event.sender_profile(), event.sender());
            (event.sender(), clean_name(Some(&name), event.sender()))
        })
        .collect()
}

fn receipts(event: &EventTimelineItem) -> impl Iterator<Item = (&UserId, Stamp)> {
    event
        .read_receipts()
        .iter()
        .map(|(user, receipt)| (user.as_ref(), receipt.ts))
}

/// The receipts to show under a message: not ours, not its sender's; the newest first
/// and at most `ReadBy::KEPT` of them. Also says how many there are in all.
fn pick<'a>(
    receipts: impl Iterator<Item = (&'a UserId, Stamp)>,
    own: &UserId,
    sender: &UserId,
) -> (Vec<(&'a UserId, Stamp)>, u32) {
    let mut others: Vec<(&UserId, Stamp)> = receipts
        .filter(|(user, _)| *user != own && *user != sender)
        .collect();
    let total = others.len().min(u32::MAX as usize) as u32;
    // Without a time last; the same time in a fixed order, so nothing flickers.
    others.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    others.truncate(ReadBy::KEPT);
    (others, total)
}

/// A name that is safe to show and not absurdly long; the user id's first part
/// where there is none.
fn clean_name(name: Option<&str>, user: &UserId) -> String {
    let name = name.map(sanitize_line).unwrap_or_default();
    let name = name.trim();
    let name = if name.is_empty() {
        sanitize_line(user.localpart())
    } else {
        name.to_owned()
    };
    truncate(&name, NAME_WIDTH)
}

fn local_time(timestamp: MilliSecondsSinceUnixEpoch) -> Option<DateTime<Local>> {
    let millis = i64::try_from(u64::from(timestamp.get())).ok()?;
    Local.timestamp_millis_opt(millis).single()
}

/// "14:05" today, otherwise with the day.
fn time_label(when: &DateTime<Local>, now: &DateTime<Local>) -> String {
    let time = when.format("%H:%M");
    match day_label(when, now).as_str() {
        "Today" => time.to_string(),
        day => format!("{day} {time}"),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use matrix_sdk::ruma::{UInt, user_id};

    use super::*;

    fn stamp(millis: u32) -> Stamp {
        Some(MilliSecondsSinceUnixEpoch(UInt::from(millis)))
    }

    #[test]
    fn our_own_receipt_and_the_senders_are_left_out() {
        let own = user_id!("@me:example.org");
        let sender = user_id!("@alice:example.org");
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let dave = user_id!("@dave:example.org");
        let receipts = [
            (own, stamp(50)),
            (bob, stamp(10)),
            (sender, stamp(40)),
            (dave, None),
            (carol, stamp(30)),
        ];
        let (picked, total) = pick(receipts.into_iter(), own, sender);
        assert_eq!(total, 3);
        // Newest first; one without a time goes last.
        let order: Vec<&UserId> = picked.iter().map(|(user, _)| *user).collect();
        assert_eq!(order, [carol, bob, dave]);
        // A message of ours that only we have "read": nobody.
        let (picked, total) = pick([(own, stamp(1))].into_iter(), own, own);
        assert!(picked.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn many_readers_are_counted_but_only_some_are_kept() {
        let own = user_id!("@me:example.org");
        let users: Vec<OwnedUserId> = (0..ReadBy::KEPT as u32 + 15)
            .map(|n| UserId::parse(format!("@user{n:03}:example.org")).unwrap())
            .collect();
        let receipts = users
            .iter()
            .enumerate()
            .map(|(n, user)| (user.as_ref(), stamp(n as u32)));
        let (picked, total) = pick(receipts, own, own);
        assert_eq!(total as usize, ReadBy::KEPT + 15);
        assert_eq!(picked.len(), ReadBy::KEPT);
        // The ones who read it last.
        assert_eq!(picked[0].0, users.last().unwrap().as_ref() as &UserId);
        // Same time: by user id, whatever order they came in.
        let same = users.iter().rev().map(|user| (user.as_ref(), stamp(7)));
        let (picked, _) = pick(same, own, own);
        assert_eq!(picked[0].0, users[0].as_ref() as &UserId);
    }

    #[test]
    fn names_are_made_safe_and_bounded() {
        let user = user_id!("@bob:example.org");
        assert_eq!(clean_name(Some("Bob\u{1b}[31m\nB"), user), "Bob[31m B");
        assert_eq!(clean_name(Some(" \u{7} "), user), "bob");
        assert_eq!(clean_name(None, user), "bob");
        let long = clean_name(Some(&"x".repeat(500)), user);
        assert!(crate::text::width(&long) <= NAME_WIDTH, "{long}");
        // Nothing that reorders the line either.
        assert!(!clean_name(Some("a\u{202e}b"), user).contains('\u{202e}'));
    }

    #[test]
    fn times_name_the_day_unless_it_is_today() {
        let now = Local.with_ymd_and_hms(2026, 11, 12, 15, 0, 0).unwrap();
        assert_eq!(time_label(&(now - Duration::hours(1)), &now), "14:00");
        assert_eq!(
            time_label(&(now - Duration::hours(20)), &now),
            "Yesterday 19:00"
        );
    }

    /// Bob reads a message of the test user with a plain SDK client; the worker then
    /// reports Bob as a reader of exactly that message.
    /// `MATRIX_RAT_TEST_USER=alice MATRIX_RAT_TEST_PASSWORD=alicepass cargo test -- --ignored`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a local homeserver"]
    async fn a_receipt_of_someone_else_names_them_under_the_message() {
        use matrix_sdk::Client;
        use matrix_sdk::config::SyncSettings;
        use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
        use matrix_sdk::ruma::events::receipt::ReceiptThread;
        use matrix_sdk::ruma::{EventId, RoomId};

        use crate::matrix::{MatrixCmd, MatrixEvent, RoomCategory, spawn};
        use crate::model::ItemRef;
        use crate::paths::Paths;

        let var = |name: &str| std::env::var(name).ok();
        let homeserver =
            var("MATRIX_RAT_TEST_HOMESERVER").unwrap_or_else(|| "http://127.0.0.1:6167".into());
        let username = var("MATRIX_RAT_TEST_USER").expect("MATRIX_RAT_TEST_USER");
        let password = var("MATRIX_RAT_TEST_PASSWORD").expect("MATRIX_RAT_TEST_PASSWORD");
        let other_name = var("MATRIX_RAT_TEST_USER2").unwrap_or_else(|| "bob".into());
        let other_password = var("MATRIX_RAT_TEST_PASSWORD2").unwrap_or_else(|| "bobpass".into());

        let other = Client::builder()
            .homeserver_url(&homeserver)
            .build()
            .await
            .expect("client");
        other
            .matrix_auth()
            .login_username(&other_name, &other_password)
            .initial_device_display_name("matrix-rat receipt test")
            .send()
            .await
            .expect("login of the second user");
        other
            .sync_once(SyncSettings::default())
            .await
            .expect("sync");

        let root = std::env::temp_dir().join(format!("matrix-rat-receipt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            log_dir: root.join("log"),
            runtime_dir: root.join("run"),
            profile: "it".into(),
        };
        paths.create_dirs().unwrap();
        let (handle, mut events) = spawn(paths);
        handle.send(MatrixCmd::LoginPassword {
            homeserver,
            username,
            password,
        });

        // A room both have joined.
        let find_room = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        let shared = rooms.iter().find(|summary| {
                            summary.category != RoomCategory::Invite
                                && RoomId::parse(&summary.id)
                                    .ok()
                                    .and_then(|id| other.get_room(&id))
                                    .is_some_and(|room| {
                                        room.state() == matrix_sdk::RoomState::Joined
                                    })
                        });
                        if let Some(summary) = shared {
                            return summary.id.clone();
                        }
                    }
                    _ => {}
                }
            }
        };
        let room_id = tokio::time::timeout(std::time::Duration::from_secs(30), find_room)
            .await
            .expect("no room that both users have joined");
        let other_room = other.get_room(&RoomId::parse(&room_id).unwrap()).unwrap();

        handle.send(MatrixCmd::OpenRoom {
            room_id: room_id.clone(),
            generation: 1,
        });
        let text = format!("receipt test {}", std::process::id());
        handle.send(MatrixCmd::Send {
            room_id: room_id.clone(),
            message: crate::format::parse(&text),
            reply_to: None,
        });

        // Waits for a state of the room's messages that `check` accepts.
        async fn until<T>(
            events: &mut tokio::sync::mpsc::UnboundedReceiver<MatrixEvent>,
            what: &str,
            check: impl Fn(&[Message]) -> Option<T>,
        ) -> T {
            let wait = async {
                loop {
                    match events.recv().await.expect("worker stopped") {
                        MatrixEvent::Timeline { messages, .. } => {
                            if let Some(found) = check(&messages) {
                                return found;
                            }
                        }
                        MatrixEvent::ActionFailed(err) => panic!("{err}"),
                        _ => {}
                    }
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(30), wait)
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        }

        let event_id = until(&mut events, "our message to be sent", |messages| {
            let message = messages.iter().find(|m| m.body == text)?;
            // Nobody but us has seen it yet.
            assert!(message.read_by.is_empty(), "{:?}", message.read_by);
            match &message.item {
                Some(ItemRef::Event(id)) => Some(id.clone()),
                _ => None,
            }
        })
        .await;

        // Bob reads it.
        other
            .sync_once(SyncSettings::default())
            .await
            .expect("sync");
        other_room
            .send_single_receipt(
                ReceiptType::Read,
                ReceiptThread::Unthreaded,
                EventId::parse(&event_id).unwrap(),
            )
            .await
            .expect("receipt of the second user");

        let reader = until(&mut events, "the reader under our message", |messages| {
            let message = messages.iter().find(|m| m.body == text)?;
            let reader = message.read_by.readers.first()?.clone();
            assert_eq!(message.read_by.total, 1);
            // On this message only.
            for other in messages.iter().filter(|m| m.body != text) {
                assert!(
                    other.read_by.readers.iter().all(|r| r.name != reader.name),
                    "{:?}",
                    other.read_by
                );
            }
            Some(reader)
        })
        .await;
        assert!(
            reader.name.to_lowercase().contains(&other_name),
            "{}",
            reader.name
        );
        assert!(!reader.time.is_empty(), "the receipt carries its time");

        handle.send(MatrixCmd::Shutdown);
        while events.recv().await.is_some() {}
        let _ = other.matrix_auth().logout().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
