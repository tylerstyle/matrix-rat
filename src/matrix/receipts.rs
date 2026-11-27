//! Who has read up to where: the read receipts the SDK attaches to timeline items,
//! turned into names on the messages the UI shows.
//!
//! Only what other people published is used. Our own receipt and the sender's own
//! (everyone has "read" what they wrote) are left out. A room's timeline carries the
//! receipts of the room, a thread's those of the thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Local, TimeZone};
use matrix_sdk::Room;
use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedUserId, UserId};
use matrix_sdk_ui::eyeball_im::Vector;
use matrix_sdk_ui::timeline::{EventTimelineItem, TimelineItem, TimelineItemContent};

use super::convert::{Deleter, ambiguous, day_label, deleter, display_name};
use crate::model::{DeletedBy, Deletion, Message, ReadBy, Reader, disambiguated};
use crate::text::{is_blank, sanitize_line, truncate};

/// Names looked up in the member list are remembered up to this many; beyond that
/// the ones looked up longest ago go first.
const NAMES_MAX: usize = 2000;
/// Lookups in one go. What is left over is named by its id for now and looked up
/// with the next change.
const LOOKUPS_MAX: usize = 300;
/// Longest name shown, in columns.
const NAME_WIDTH: usize = 40;

type Stamp = Option<MilliSecondsSinceUnixEpoch>;

/// The receipts shown on one timeline item, and who deleted it.
struct Picked<'a> {
    /// The item's unique id, which is the message's id.
    id: &'a str,
    readers: Vec<(&'a UserId, Stamp)>,
    total: u32,
    /// Who deleted it, where that was someone else than its sender and than us: named
    /// like a reader (see `convert::deleter`).
    deleter: Option<OwnedUserId>,
}

/// A name from the member list. `found` is false for a stand-in (the id's first
/// part) used because the list did not have the person.
struct Known {
    name: String,
    found: bool,
    /// Someone else in the room goes by the same name.
    ambiguous: bool,
}

/// Turns receipts into names, and remembers both between the timeline's changes:
/// most changes move no receipt, and then nothing is worked out again.
#[derive(Default)]
pub struct Names {
    /// Readers who have not written anything that is loaded (those who have are
    /// named by their messages), and the order they were looked up in.
    known: HashMap<OwnedUserId, Known>,
    order: VecDeque<OwnedUserId>,
    /// What the last pass made, by message id, and a digest of the receipts (and the
    /// deleters) it was made from.
    made: HashMap<String, ReadBy>,
    /// Who deleted a message and their name as shown, by message id, from the last pass.
    deleters: HashMap<String, (OwnedUserId, String)>,
    digest: Option<u64>,
    /// A digest of the timeline's changes of membership and profile at the last pass.
    /// A new one may have changed anyone's name, or who shares it: what was looked
    /// up is looked up again, and those who wrote something are named anew.
    members: Option<u64>,
    /// The member list was complete at the last pass.
    synced: bool,
    /// The last pass left names to look up.
    unfinished: bool,
}

impl Names {
    /// Fills in `read_by` on the messages made from `items` (matched by their id), and
    /// the name of whoever deleted one where that was someone else than its sender.
    /// Names come from the local store only: a receipt is not worth a request. Those
    /// `ignored` are not shown as readers.
    pub async fn annotate(
        &mut self,
        room: &Room,
        messages: &mut [Message],
        items: &Vector<Arc<TimelineItem>>,
        own: &UserId,
        ignored: &HashSet<OwnedUserId>,
    ) {
        // One pass over the items for the receipts, and for the changes of membership
        // or profile that may have changed names; names only if either changed.
        let mut members = std::collections::hash_map::DefaultHasher::new();
        let picked: Vec<Picked> = items
            .iter()
            .filter_map(|item| {
                let event = item.as_event()?;
                if matches!(
                    event.content(),
                    TimelineItemContent::MembershipChange(_)
                        | TimelineItemContent::ProfileChange(_)
                ) {
                    item.unique_id().0.hash(&mut members);
                }
                let (readers, total) = pick(receipts(event), own, event.sender(), ignored);
                let moderator = match event.content().is_redacted().then(|| deleter(event, own)) {
                    Some(Deleter::Other(user)) => Some(user),
                    _ => None,
                };
                (total > 0 || moderator.is_some()).then(|| Picked {
                    id: item.unique_id().0.as_str(),
                    readers,
                    total,
                    deleter: moderator,
                })
            })
            .collect();
        if let Some(needed) = self.plan(&picked, room.are_members_synced(), members.finish()) {
            // Whoever wrote something loaded is named by it, the newest first.
            let mut speakers: HashMap<&UserId, (String, bool)> = HashMap::new();
            for event in items.iter().rev().filter_map(|item| item.as_event()) {
                let sender = event.sender();
                if needed.contains(sender) && !speakers.contains_key(sender) {
                    let profile = event.sender_profile();
                    let name = clean_name(Some(&display_name(profile, sender)), sender);
                    speakers.insert(sender, (name, ambiguous(profile)));
                }
            }
            for user in self.wanted(&needed, &speakers) {
                let (name, ambiguous) = match room.get_member_no_sync(user).await {
                    Ok(Some(member)) => (
                        member.display_name().map(str::to_owned),
                        member.name_ambiguous(),
                    ),
                    _ => (None, false),
                };
                self.learn(user, name.as_deref(), ambiguous);
            }
            self.make(&picked, &speakers, &Local::now());
        }
        for message in messages {
            message.read_by = self
                .made
                .get(message.id.as_str())
                .cloned()
                .unwrap_or_default();
            // A message made before (see `convert::Reuse`) gets the name of now too.
            if let Some(Deletion {
                by: DeletedBy::Other { user_id, name },
                ..
            }) = &mut message.deleted
                && let Some((user, shown)) = self.deleters.get(message.id.as_str())
                && user.as_str() == user_id.as_str()
            {
                name.clone_from(shown);
            }
        }
    }

    /// Decides whether names have to be worked out: `None` if the receipts, the
    /// deleters and the changes of membership (`members`, a digest) are the ones of the
    /// last pass and nothing was left open; otherwise everyone to name. A deletion
    /// changes no receipt: without its deleter in the digest it would not be named.
    fn plan<'p>(
        &mut self,
        picked: &'p [Picked<'_>],
        synced: bool,
        members: u64,
    ) -> Option<HashSet<&'p UserId>> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for item in picked {
            item.id.hash(&mut hasher);
            item.total.hash(&mut hasher);
            for (user, stamp) in &item.readers {
                user.hash(&mut hasher);
                stamp.hash(&mut hasher);
            }
            item.deleter.hash(&mut hasher);
        }
        let digest = hasher.finish();
        // The member list has arrived since: stand-ins from before get their names.
        let members_arrived = synced && !self.synced;
        self.synced = synced;
        if members_arrived {
            self.known.retain(|_, known| known.found);
            self.order.retain(|user| self.known.contains_key(user));
        }
        // Someone joined, left or changed their name since: anyone's name may be
        // another now, or someone else's too.
        let renamed = self.members.is_some_and(|before| before != members);
        self.members = Some(members);
        if renamed {
            self.known.clear();
            self.order.clear();
        }
        if self.digest == Some(digest) && !members_arrived && !renamed && !self.unfinished {
            return None;
        }
        self.digest = Some(digest);
        Some(
            picked
                .iter()
                .flat_map(|item| {
                    let readers = item.readers.iter().map(|(user, _)| *user);
                    readers.chain(item.deleter.as_deref())
                })
                .collect(),
        )
    }

    /// Whom to look up in the member list now: readers named neither by a message
    /// nor from an earlier lookup. No more than `LOOKUPS_MAX` at a time.
    fn wanted<'a>(
        &mut self,
        needed: &HashSet<&'a UserId>,
        speakers: &HashMap<&UserId, (String, bool)>,
    ) -> Vec<&'a UserId> {
        let mut wanted: Vec<&UserId> = needed
            .iter()
            .copied()
            .filter(|user| !speakers.contains_key(user) && !self.known.contains_key(*user))
            .collect();
        // A fixed order, so that what is left over is not the same people each time.
        wanted.sort_unstable();
        self.unfinished = wanted.len() > LOOKUPS_MAX;
        wanted.truncate(LOOKUPS_MAX);
        wanted
    }

    /// Remembers what the member list says about `user` (`None`: nothing), and whether
    /// someone else goes by the same name.
    fn learn(&mut self, user: &UserId, name: Option<&str>, ambiguous: bool) {
        let known = Known {
            name: clean_name(name, user),
            found: name.is_some(),
            ambiguous,
        };
        if self.known.insert(user.to_owned(), known).is_none() {
            self.order.push_back(user.to_owned());
        }
        // The oldest make room, one at a time: never everything at once.
        while self.known.len() > NAMES_MAX {
            match self.order.pop_front() {
                Some(oldest) => self.known.remove(&oldest),
                None => break,
            };
        }
    }

    fn make(
        &mut self,
        picked: &[Picked],
        speakers: &HashMap<&UserId, (String, bool)>,
        now: &DateTime<Local>,
    ) {
        // Named as a reader is, but by the user ID where no name is known: who deleted
        // a message is not left to a stand-in.
        self.deleters = picked
            .iter()
            .filter_map(|item| {
                let user = item.deleter.as_deref()?;
                let (name, ambiguous) = match (speakers.get(user), self.known.get(user)) {
                    (Some((name, ambiguous)), _) => (name.clone(), *ambiguous),
                    (None, Some(known)) if known.found => (known.name.clone(), known.ambiguous),
                    (None, _) => (sanitize_line(user.as_str()), false),
                };
                let shown = if ambiguous {
                    disambiguated(&name, user.as_str())
                } else {
                    name
                };
                Some((item.id.to_owned(), (user.to_owned(), shown)))
            })
            .collect();
        self.made = picked
            .iter()
            .filter(|item| item.total > 0)
            .map(|item| {
                let readers = item
                    .readers
                    .iter()
                    .map(|(user, stamp)| {
                        let (name, ambiguous) = match (speakers.get(user), self.known.get(*user)) {
                            (Some((name, ambiguous)), _) => (name.clone(), *ambiguous),
                            (None, Some(known)) => (known.name.clone(), known.ambiguous),
                            (None, None) => (clean_name(None, user), false),
                        };
                        Reader {
                            name,
                            id: Some(user.to_string()),
                            ambiguous,
                            time: stamp
                                .and_then(local_time)
                                .map(|when| time_label(&when, now))
                                .unwrap_or_default(),
                        }
                    })
                    .collect();
                let read_by = ReadBy {
                    readers,
                    total: item.total,
                };
                (item.id.to_owned(), read_by)
            })
            .collect();
    }
}

fn receipts(event: &EventTimelineItem) -> impl Iterator<Item = (&UserId, Stamp)> {
    event
        .read_receipts()
        .iter()
        .map(|(user, receipt)| (user.as_ref(), receipt.ts))
}

/// The receipts to show under a message: not ours, not its sender's, not those of
/// someone `ignored`; the newest first and at most `ReadBy::KEPT` of them. Also says
/// how many there are in all.
fn pick<'a>(
    receipts: impl Iterator<Item = (&'a UserId, Stamp)>,
    own: &UserId,
    sender: &UserId,
    ignored: &HashSet<OwnedUserId>,
) -> (Vec<(&'a UserId, Stamp)>, u32) {
    let mut others: Vec<(&UserId, Stamp)> = receipts
        .filter(|(user, _)| *user != own && *user != sender && !ignored.contains(*user))
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
    let name = if is_blank(&name) {
        sanitize_line(user.localpart())
    } else {
        name.trim().to_owned()
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
        let none = HashSet::new();
        let (picked, total) = pick(receipts.into_iter(), own, sender, &none);
        assert_eq!(total, 3);
        // Newest first; one without a time goes last.
        let order: Vec<&UserId> = picked.iter().map(|(user, _)| *user).collect();
        assert_eq!(order, [carol, bob, dave]);
        // Nor anyone ignored, who is not counted either.
        let ignored = HashSet::from([bob.to_owned()]);
        let (picked, total) = pick(receipts.into_iter(), own, sender, &ignored);
        assert_eq!(total, 2);
        let order: Vec<&UserId> = picked.iter().map(|(user, _)| *user).collect();
        assert_eq!(order, [carol, dave]);
        // A message of ours that only we have "read": nobody.
        let (picked, total) = pick([(own, stamp(1))].into_iter(), own, own, &none);
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
        let none = HashSet::new();
        let (picked, total) = pick(receipts, own, own, &none);
        assert_eq!(total as usize, ReadBy::KEPT + 15);
        assert_eq!(picked.len(), ReadBy::KEPT);
        // The ones who read it last.
        assert_eq!(picked[0].0, users.last().unwrap().as_ref() as &UserId);
        // Same time: by user id, whatever order they came in.
        let same = users.iter().rev().map(|user| (user.as_ref(), stamp(7)));
        let (picked, _) = pick(same, own, own, &none);
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
    fn a_name_that_shows_nothing_gets_the_id_part() {
        let user = user_id!("@bob:example.org");
        for hidden in [
            "\u{200b}",
            "\u{200d}\u{feff} ",
            "\u{3164}",
            "\u{200b}\u{202e}",
        ] {
            assert_eq!(clean_name(Some(hidden), user), "bob", "{hidden:?}");
        }
        assert_eq!(clean_name(Some(" b\u{200b}ob "), user), "b\u{200b}ob");
    }

    fn picked<'a>(id: &'a str, readers: &[&'a UserId]) -> Picked<'a> {
        Picked {
            id,
            readers: readers.iter().map(|user| (*user, stamp(5))).collect(),
            total: readers.len() as u32,
            deleter: None,
        }
    }

    /// A pass as `annotate` does it, with `members` standing in for the store.
    /// Returns how many lookups were made; `None` if nothing was worked out.
    fn pass(
        names: &mut Names,
        items: &[Picked],
        speakers: &HashMap<&UserId, (String, bool)>,
        members: &HashMap<&UserId, &str>,
        synced: bool,
    ) -> Option<usize> {
        pass_after(names, items, speakers, members, synced, 0)
    }

    /// [`pass`] with `changes` for the digest of the timeline's member changes.
    fn pass_after(
        names: &mut Names,
        items: &[Picked],
        speakers: &HashMap<&UserId, (String, bool)>,
        members: &HashMap<&UserId, &str>,
        synced: bool,
        changes: u64,
    ) -> Option<usize> {
        let needed = names.plan(items, synced, changes)?;
        let wanted = names.wanted(&needed, speakers);
        for user in &wanted {
            names.learn(user, members.get(user).copied(), false);
        }
        names.make(items, speakers, &Local::now());
        Some(wanted.len())
    }

    fn named(names: &Names, id: &str) -> Vec<String> {
        let read_by = names.made.get(id).cloned().unwrap_or_default();
        read_by.readers.into_iter().map(|r| r.name).collect()
    }

    #[test]
    fn names_come_from_messages_first_and_are_worked_out_once() {
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let speakers = HashMap::from([(bob, ("Bob the Speaker".to_owned(), false))]);
        let members = HashMap::from([(bob, "Bobby"), (carol, "Carol")]);
        let mut names = Names::default();
        let items = [picked("m1", &[bob, carol])];
        // Only carol has to be looked up; bob is named by what he wrote.
        assert_eq!(pass(&mut names, &items, &speakers, &members, true), Some(1));
        assert_eq!(named(&names, "m1"), ["Bob the Speaker", "Carol"]);
        // The same receipts again (a new message, an edit): nothing is redone.
        assert_eq!(pass(&mut names, &items, &speakers, &members, true), None);
        assert_eq!(named(&names, "m1"), ["Bob the Speaker", "Carol"]);
        // A receipt moves: worked out again, without a second lookup.
        let moved = [picked("m1", &[bob]), picked("m2", &[carol])];
        assert_eq!(pass(&mut names, &moved, &speakers, &members, true), Some(0));
        assert_eq!(named(&names, "m1"), ["Bob the Speaker"]);
        assert_eq!(named(&names, "m2"), ["Carol"]);
        // Nobody reads anything any more.
        assert_eq!(pass(&mut names, &[], &speakers, &members, true), Some(0));
        assert!(names.made.is_empty());
    }

    /// Two readers of the same name (here without display names, as the SDK counts
    /// the first part of the id then) are told apart in a list of readers, and their
    /// colours come from their ids.
    #[test]
    fn readers_who_share_a_name_carry_their_ids() {
        let here = user_id!("@samuelsson:home1.test");
        let there = user_id!("@samuelsson:home2.test");
        let bob = user_id!("@bob:example.org");
        // One of them wrote something; the other is looked up.
        let speakers = HashMap::from([(here, ("samuelsson".to_owned(), true))]);
        let mut names = Names::default();
        let items = [picked("m1", &[here, there, bob])];
        let needed = names.plan(&items, true, 0).unwrap();
        for user in names.wanted(&needed, &speakers) {
            names.learn(user, None, user != bob);
        }
        names.make(&items, &speakers, &Local::now());
        let readers = &names.made["m1"].readers;
        let shown: Vec<String> = readers.iter().map(|r| r.shown_name().into()).collect();
        assert_eq!(
            shown,
            [
                "samuelsson (@samuelsson:home1.test)",
                "samuelsson (@samuelsson:home2.test)",
                "bob"
            ]
        );
        let keys: Vec<&str> = readers.iter().map(Reader::key).collect();
        assert_eq!(keys, [here.as_str(), there.as_str(), bob.as_str()]);
    }

    /// A reader who changes their name, or takes one someone else has, is named
    /// anew although no receipt moved: the change of their membership shows in the
    /// timeline, and what was looked up is looked up again.
    #[test]
    fn readers_are_named_anew_after_a_change_of_membership() {
        let bob = user_id!("@bob:example.org");
        let nobody = HashMap::new();
        let mut names = Names::default();
        let items = [picked("m1", &[bob])];
        let before = HashMap::from([(bob, "Bob")]);
        assert_eq!(
            pass_after(&mut names, &items, &nobody, &before, true, 1),
            Some(1)
        );
        assert_eq!(named(&names, "m1"), ["Bob"]);
        // The same receipts, the same changes: nothing is redone.
        let after = HashMap::from([(bob, "Robert")]);
        assert_eq!(
            pass_after(&mut names, &items, &nobody, &after, true, 1),
            None
        );
        // A new change of membership: looked up again.
        assert_eq!(
            pass_after(&mut names, &items, &nobody, &after, true, 2),
            Some(1)
        );
        assert_eq!(named(&names, "m1"), ["Robert"]);
    }

    #[test]
    fn stand_ins_are_replaced_once_the_member_list_is_there() {
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let nobody = HashMap::new();
        let mut names = Names::default();
        let items = [picked("m1", &[bob, carol])];
        // Before the list is complete only carol is in the store.
        let early = HashMap::from([(carol, "Carol")]);
        assert_eq!(pass(&mut names, &items, &nobody, &early, false), Some(2));
        assert_eq!(named(&names, "m1"), ["bob", "Carol"]);
        assert_eq!(pass(&mut names, &items, &nobody, &early, false), None);
        // The list arrives, the receipts are the same: bob is asked about again,
        // carol is not.
        let full = HashMap::from([(bob, "Bobby Tables"), (carol, "Carol")]);
        assert_eq!(pass(&mut names, &items, &nobody, &full, true), Some(1));
        assert_eq!(named(&names, "m1"), ["Bobby Tables", "Carol"]);
        // Someone the complete list does not have keeps the stand-in, for good.
        let dave = user_id!("@dave:example.org");
        let items = [picked("m1", &[bob, carol, dave])];
        assert_eq!(pass(&mut names, &items, &nobody, &full, true), Some(1));
        assert_eq!(named(&names, "m1"), ["Bobby Tables", "Carol", "dave"]);
        let items = [picked("m2", &[bob, carol, dave])];
        assert_eq!(pass(&mut names, &items, &nobody, &full, true), Some(0));
    }

    #[test]
    fn many_readers_are_looked_up_in_portions_and_the_oldest_names_make_room() {
        let users: Vec<OwnedUserId> = (0..NAMES_MAX + LOOKUPS_MAX + 50)
            .map(|n| UserId::parse(format!("@user{n:05}:example.org")).unwrap())
            .collect();
        let nobody = HashMap::new();
        let members = HashMap::new();
        let mut names = Names::default();
        // More unknown readers at once than one pass looks up.
        let crowd: Vec<&UserId> = users[..LOOKUPS_MAX + 50]
            .iter()
            .map(|u| u.as_ref())
            .collect();
        let items = [picked("m1", &crowd)];
        assert_eq!(
            pass(&mut names, &items, &nobody, &members, true),
            Some(LOOKUPS_MAX)
        );
        // Everyone has a name meanwhile, and the rest follows with the next change,
        // even though the receipts are the same.
        assert_eq!(named(&names, "m1").len(), LOOKUPS_MAX + 50);
        assert_eq!(pass(&mut names, &items, &nobody, &members, true), Some(50));
        assert_eq!(pass(&mut names, &items, &nobody, &members, true), None);

        // Past the limit only as many go as come: the newest stay known.
        for chunk in users[LOOKUPS_MAX + 50..].chunks(LOOKUPS_MAX) {
            let readers: Vec<&UserId> = chunk.iter().map(|u| u.as_ref()).collect();
            let items = [picked("m1", &readers)];
            assert_eq!(
                pass(&mut names, &items, &nobody, &members, true),
                Some(chunk.len())
            );
            assert!(names.known.len() <= NAMES_MAX);
            assert_eq!(names.known.len(), names.order.len());
        }
        assert_eq!(names.known.len(), NAMES_MAX);
        assert!(names.known.contains_key(users.last().unwrap()));
        assert!(!names.known.contains_key(&users[0]));
        // The last readers again: all still known, nothing looked up.
        let last: Vec<&UserId> = users[users.len() - 100..]
            .iter()
            .map(|u| u.as_ref())
            .collect();
        assert_eq!(
            pass(&mut names, &[picked("m2", &last)], &nobody, &members, true),
            Some(0)
        );
    }

    /// A deletion by a moderator changes no receipt: the moderator is named all the
    /// same, by what they wrote, by the member list, or else by their user ID, and the
    /// name stays on the message in the passes after.
    #[test]
    fn a_deletion_without_a_change_of_receipts_names_the_moderator() {
        let bob = user_id!("@bob:example.org");
        let carol = user_id!("@carol:example.org");
        let dave = user_id!("@dave:example.org");
        let nobody = HashMap::new();
        let members = HashMap::from([(carol, "Carol"), (bob, "Bob")]);
        let mut names = Names::default();
        let items = [picked("m1", &[bob])];
        assert_eq!(pass(&mut names, &items, &nobody, &members, true), Some(1));
        assert!(names.deleters.is_empty());
        // Carol deletes m2, which nobody has read: the receipts are the same.
        let deleted = Picked {
            deleter: Some(carol.to_owned()),
            ..picked("m2", &[])
        };
        let items = [picked("m1", &[bob]), deleted];
        assert_eq!(pass(&mut names, &items, &nobody, &members, true), Some(1));
        assert_eq!(names.deleters["m2"], (carol.to_owned(), "Carol".to_owned()));
        // A message without readers has none listed.
        assert!(!names.made.contains_key("m2"));
        // Nothing changed: nothing worked out again, the name is kept.
        assert_eq!(pass(&mut names, &items, &nobody, &members, true), None);
        assert_eq!(names.deleters["m2"].1, "Carol");
        // Someone the member list does not know is named by their user ID.
        let unknown = Picked {
            deleter: Some(dave.to_owned()),
            ..picked("m3", &[])
        };
        assert_eq!(
            pass(&mut names, &[unknown], &nobody, &members, true),
            Some(1)
        );
        assert_eq!(names.deleters["m3"].1, "@dave:example.org");
        // One who shares a name with someone else, as the SDK says, carries the ID.
        let mut names = Names::default();
        let speakers = HashMap::from([(carol, ("carol".to_owned(), true))]);
        let deleted = Picked {
            deleter: Some(carol.to_owned()),
            ..picked("m2", &[])
        };
        assert_eq!(
            pass(&mut names, &[deleted], &speakers, &members, true),
            Some(0)
        );
        assert_eq!(names.deleters["m2"].1, "carol (@carol:example.org)");
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

        use crate::matrix::{MatrixCmd, MatrixEvent, spawn};
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
            password: password.into(),
        });

        // A room both have joined.
        let find_room = async {
            loop {
                match events.recv().await.expect("worker stopped") {
                    MatrixEvent::LoginFailed(err) | MatrixEvent::Error(err) => panic!("{err}"),
                    MatrixEvent::Rooms(rooms) => {
                        let shared = rooms.iter().find(|summary| {
                            !summary.is_pending()
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
