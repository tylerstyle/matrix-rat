//! Slash commands that act on rooms and people: `/join`, `/kick`, `/topic` and the
//! rest. Each is another way into something the app already does elsewhere, with the
//! same checks and the same questions; nothing here talks to the server by itself.

use super::App;
use super::people::checked_display_name;
use super::rooms::{NO_ACCOUNT, room_text_change};
use crate::action::Action;
use crate::complete::{Member, MentionKind, MentionMark};
use crate::manage::{Address, Confirm, InfoRow, Stage, clean_input, parse_address, parse_user};
use crate::matrix::{MatrixCmd, fallback_localpart};
use crate::modal::{Modal, ReasonConfirmPopup};
use crate::people::UserAction;
use crate::text::{sanitize_line, truncate};

/// The commands, by what is typed after the slash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RoomCommand {
    Join,
    Leave,
    Invite,
    Kick,
    Ban,
    Unban,
    Ignore,
    Unignore,
    Nick,
    RoomNick,
    Topic,
    RoomName,
    DirectChat,
    Whois,
    Help,
}

impl RoomCommand {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "join" => Self::Join,
            "leave" => Self::Leave,
            "invite" => Self::Invite,
            "kick" => Self::Kick,
            "ban" => Self::Ban,
            "unban" => Self::Unban,
            "ignore" => Self::Ignore,
            "unignore" => Self::Unignore,
            "nick" => Self::Nick,
            "roomnick" => Self::RoomNick,
            "topic" => Self::Topic,
            "roomname" => Self::RoomName,
            "dm" | "msg" => Self::DirectChat,
            "whois" => Self::Whois,
            "help" => Self::Help,
            _ => return None,
        })
    }

    /// What goes after the name, for when it is missing.
    fn usage(self, name: &str) -> String {
        let (what, form) = match self {
            Self::Join => ("a room", "<#room:server, room ID or link>"),
            Self::Invite | Self::Ignore | Self::Unignore | Self::DirectChat | Self::Whois => {
                ("someone", "<user>")
            }
            Self::Kick | Self::Ban | Self::Unban => ("someone", "<user> [reason]"),
            Self::Nick => ("a name", "<display name>"),
            Self::RoomNick => ("a name", "<your name in this room>"),
            Self::Topic => ("some text", "<new topic>"),
            Self::RoomName => ("a name", "<new room name>"),
            Self::Leave => return "/leave takes nothing after it: it leaves the open room".into(),
            Self::Help => return "/help takes nothing after it".into(),
        };
        format!("/{name} needs {what}: /{name} {form}")
    }

    /// Whether a person's name may be followed by more (a reason).
    fn takes_reason(self) -> bool {
        matches!(self, Self::Kick | Self::Ban | Self::Unban)
    }
}

/// A room command typed into the composer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Typed<'a> {
    pub command: RoomCommand,
    /// As typed (`dm` or `msg`), for the hints.
    pub name: &'a str,
    /// What follows the name, without the spaces around it.
    pub arg: &'a str,
    /// Where `arg` starts in the text, to match accepted mentions against it.
    pub arg_start: usize,
}

/// Hand-written: what was typed after the command (names, reasons) stays out of logs.
impl std::fmt::Debug for Typed<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Typed")
            .field("command", &self.command)
            .field("arg_bytes", &self.arg.len())
            .finish_non_exhaustive()
    }
}

/// The room command `text` is, if it is one. `//` sends text that starts with a slash
/// and is never a command.
pub(super) fn parse(text: &str) -> Option<Typed<'_>> {
    let start = text.len() - text.trim_start().len();
    let rest = text[start..].strip_prefix('/')?;
    let name_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_len];
    let command = RoomCommand::from_name(name)?;
    let after = &rest[name_len..];
    let arg_start = start + 1 + name_len + (after.len() - after.trim_start().len());
    Some(Typed {
        command,
        name,
        arg: text[arg_start..].trim_end(),
        arg_start,
    })
}

/// The person a command names.
#[derive(Clone, PartialEq, Eq)]
struct Named {
    user_id: String,
    /// Their display name in the room, if one is known.
    name: Option<String>,
}

/// Hand-written: who it is stays out of logs.
impl std::fmt::Debug for Named {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Named")
            .field("has_name", &self.name.is_some())
            .finish_non_exhaustive()
    }
}

impl Named {
    /// How the person is shown in a question or a hint.
    fn label(&self) -> String {
        match &self.name {
            Some(name) => sanitize_line(name),
            None => self.user_id.clone(),
        }
    }

    /// Name and ID together, where it matters to be sure who it is.
    fn label_with_id(&self) -> String {
        match &self.name {
            Some(name) => format!("{} ({})", sanitize_line(name), self.user_id),
            None => self.user_id.clone(),
        }
    }
}

/// Names of more words than this are not looked for.
const MAX_NAME_WORDS: usize = 8;

/// Who `arg` names, and what follows (only when `with_rest`; otherwise all of `arg`
/// names one person). A complete user ID or link is taken as it is; anything else has
/// to be the display name or the part before the colon of exactly one member of the
/// open room. A mention accepted from the completions is taken as the person it
/// stands for. The error is a sentence for the user.
fn resolve_user<'a>(
    arg: &'a str,
    arg_start: usize,
    with_rest: bool,
    members: &[Member],
    marks: &[MentionMark],
) -> Result<(Named, &'a str), String> {
    let ends_word =
        |rest: &str| rest.is_empty() || rest.starts_with(char::is_whitespace) && with_rest;
    let named = |user_id: &str| Named {
        user_id: user_id.to_owned(),
        name: members
            .iter()
            .find(|m| m.user_id == user_id)
            .map(|m| m.display_name.clone())
            .filter(|name| !crate::text::is_blank(name)),
    };
    for mark in marks {
        if let MentionKind::User { label, user_id } = &mark.kind
            && mark.at.is_none_or(|at| at == arg_start)
            && let Some(rest) = arg.strip_prefix(label.as_str())
            && ends_word(rest)
        {
            let mut found = named(user_id);
            found.name.get_or_insert_with(|| label.clone());
            return Ok((found, rest.trim_start()));
        }
    }

    let first = arg.split_whitespace().next().unwrap_or_default();
    let lower = first.to_ascii_lowercase();
    let is_id = first.starts_with('@') && first.contains(':')
        || ["https://", "http://", "matrix:"]
            .iter()
            .any(|scheme| lower.starts_with(scheme));
    if is_id {
        let rest = arg[first.len()..].trim_start();
        if !rest.is_empty() && !with_rest {
            return Err(format!(
                "Only one person, please: {} is followed by more",
                truncate(&sanitize_line(first), 40)
            ));
        }
        let user_id = parse_user(first)?;
        return Ok((named(&user_id), rest));
    }

    // Every way the start of `arg` could be read as a name: the whole of it, or (with
    // a reason after it) the first one, two, … words.
    let mut prefixes: Vec<&str> = Vec::new();
    if with_rest {
        for (at, ch) in arg.char_indices() {
            if ch.is_whitespace() && !arg[..at].ends_with(char::is_whitespace) {
                prefixes.push(&arg[..at]);
                if prefixes.len() == MAX_NAME_WORDS {
                    break;
                }
            }
        }
    }
    prefixes.push(arg);
    // Each person who fits, with the longest name of theirs that does.
    let mut found: Vec<(&Member, usize)> = Vec::new();
    for prefix in prefixes {
        let wanted = prefix.to_lowercase();
        let local = wanted.strip_prefix('@').unwrap_or(&wanted);
        for member in members {
            let by_name = member.display_name.to_lowercase() == wanted;
            let by_local = fallback_localpart(&member.user_id).to_lowercase() == local;
            if !(by_name || by_local) {
                continue;
            }
            match found.iter_mut().find(|(m, _)| m.user_id == member.user_id) {
                Some((_, len)) => *len = (*len).max(prefix.len()),
                None => found.push((member, prefix.len())),
            }
        }
    }
    let shown = || format!("“{}”", truncate(&sanitize_line(arg), 40));
    match found.as_slice() {
        [] => Err(format!(
            "No one in this room is called {}. Use their full ID, like @name:server",
            shown()
        )),
        [(member, len)] => Ok((named(&member.user_id), arg[*len..].trim_start())),
        _ => Err(format!(
            "More than one person here could be {}. Use their full ID, like @name:server",
            shown()
        )),
    }
}

impl App {
    /// Handles a room command on Enter in the composer. `true` if the text was one,
    /// whatever became of it. A command that cannot be done says why and leaves the
    /// draft, so a typo is easy to fix.
    pub(super) fn room_command(&mut self) -> bool {
        let text = self.composer.text().to_owned();
        let Some(typed) = parse(&text) else {
            return false;
        };
        if matches!(self.compose_mode, super::ComposeMode::Edit(_)) {
            self.show_toast("Finish or cancel the edit first");
            return true;
        }
        let bare = matches!(typed.command, RoomCommand::Leave | RoomCommand::Help);
        let missing = !bare && typed.arg.is_empty();
        // `/leave #elsewhere` must not leave the open room.
        let extra = bare && !typed.arg.is_empty();
        if missing || extra {
            self.show_toast(typed.command.usage(typed.name));
            return true;
        }
        let went_ahead = match self.run_room_command(&typed) {
            Ok(()) => true,
            Err(reason) => {
                self.show_toast(reason);
                false
            }
        };
        if went_ahead {
            // As after `/poll`: the emptied composer is saved as the draft in due course.
            self.composer.set_text("");
            self.clear_completion_marks();
            self.sync_completion();
        }
        true
    }

    fn run_room_command(&mut self, typed: &Typed<'_>) -> Result<(), String> {
        match typed.command {
            RoomCommand::Join => self.join_command(typed),
            RoomCommand::Leave => {
                self.open_room_or_say()?;
                self.open_info(self.open_index(), Some(InfoRow::Leave));
                // Typing the command was the first step; the question is the second.
                if let Some(Modal::RoomInfo(info)) = &mut self.modal {
                    info.confirm = Some(Confirm::Leave);
                }
                Ok(())
            }
            RoomCommand::Invite | RoomCommand::DirectChat => {
                // Inviting needs a room to invite into; said here, so the draft stays.
                if typed.command == RoomCommand::Invite {
                    self.open_room_or_say()?;
                }
                let (person, _) = self.named(typed)?;
                let stage = if typed.command == RoomCommand::Invite {
                    Stage::Invite
                } else {
                    Stage::Direct
                };
                // The form, filled in and sent as if typed there: it checks what it
                // checks there, shows progress, and keeps a mistake to correct.
                self.open_new_with(stage, |popup| {
                    popup.user.set(person.user_id.as_str());
                });
                self.submit_new();
                Ok(())
            }
            RoomCommand::Kick | RoomCommand::Ban | RoomCommand::Unban => {
                self.moderation_command(typed)
            }
            RoomCommand::Ignore | RoomCommand::Unignore => {
                let (person, _) = self.named(typed)?;
                if self.user_id.as_deref() == Some(person.user_id.as_str()) {
                    return Err("You cannot ignore your own account".into());
                }
                let ignored = self.ignored_users.contains(&person.user_id);
                match (typed.command == RoomCommand::Ignore, ignored) {
                    (true, true) => return Err(format!("{} is ignored already", person.label())),
                    (false, false) => return Err(format!("{} is not ignored", person.label())),
                    _ => {}
                }
                let room_id = self.room_id();
                self.open_user_info_at(
                    person.user_id,
                    room_id,
                    person.name,
                    UserAction::IgnoreToggle,
                );
                Ok(())
            }
            RoomCommand::Whois => {
                let (person, _) = self.named(typed)?;
                let room_id = self.room_id();
                self.open_user_info(person.user_id, room_id, person.name);
                Ok(())
            }
            RoomCommand::Nick => {
                if self.matrix.is_none() {
                    return Err(NO_ACCOUNT.into());
                }
                let name = checked_display_name(typed.arg)?
                    .ok_or_else(|| typed.command.usage(typed.name))?;
                let current = self
                    .own_profile
                    .as_ref()
                    .and_then(|p| p.display_name.as_ref());
                if current == Some(&name) {
                    return Err("That is your display name already".into());
                }
                // The answer comes as a toast, as from the profile popup when it is closed.
                self.send_matrix(MatrixCmd::SetOwnDisplayName { name: Some(name) });
                Ok(())
            }
            RoomCommand::RoomNick | RoomCommand::Topic | RoomCommand::RoomName => {
                self.room_text_command(typed)
            }
            RoomCommand::Help => {
                if !self.help_open {
                    self.perform(Action::ToggleHelp);
                }
                Ok(())
            }
        }
    }

    /// `/join`: the room if it is joined, else its preview, from which it is joined
    /// as from a link.
    fn join_command(&mut self, typed: &Typed<'_>) -> Result<(), String> {
        // A room picked from the `#` completions stands for its link.
        let picked = self
            .complete
            .marks
            .iter()
            .find_map(|mark| match &mark.kind {
                MentionKind::RoomLink { label, uri }
                    if mark.at.is_none_or(|at| at == typed.arg_start)
                        && typed.arg == label.as_str() =>
                {
                    Some(uri.clone())
                }
                _ => None,
            });
        let address = picked.as_deref().unwrap_or(typed.arg);
        match parse_address(address)? {
            Address::Room { target, via }
            | Address::Event {
                room: target, via, ..
            } => {
                match self.find_joined_room(&target) {
                    Some(index) => self.show_room(index),
                    None => self.open_room_preview(&target, via),
                }
                Ok(())
            }
            Address::User(_) => Err("That is a person, not a room: /dm opens a chat".into()),
        }
    }

    /// `/kick`, `/ban` and `/unban`: the question the message menu asks before a ban,
    /// with the reason typed after the name already in it.
    fn moderation_command(&mut self, typed: &Typed<'_>) -> Result<(), String> {
        let room_id = self.open_room_or_say()?;
        if self.matrix.is_none() {
            return Err(NO_ACCOUNT.into());
        }
        let (person, reason) = self.named(typed)?;
        let own = self.user_id.as_deref();
        if own == Some(person.user_id.as_str()) {
            return Err(match typed.command {
                RoomCommand::Kick => "To go yourself, use /leave",
                _ => "That is you",
            }
            .into());
        }
        let power = &self.room().ok_or("No room is open")?.power;
        let label = person.label();
        let (allowed, popup): (bool, fn(String, String, String) -> ReasonConfirmPopup) =
            match typed.command {
                RoomCommand::Kick => (
                    power.can_kick_user(own, &person.user_id),
                    ReasonConfirmPopup::kick,
                ),
                RoomCommand::Ban => (
                    power.can_ban_user(own, &person.user_id),
                    ReasonConfirmPopup::ban,
                ),
                _ => (
                    power.can_unban_user(own, &person.user_id),
                    ReasonConfirmPopup::unban,
                ),
            };
        if !allowed {
            let what = match typed.command {
                RoomCommand::Kick => "remove",
                RoomCommand::Ban => "ban",
                _ => "unban",
            };
            return Err(format!("You are not allowed to {what} {label} here"));
        }
        let popup = popup(room_id, person.user_id.clone(), person.label_with_id());
        self.open_modal(Modal::ReasonConfirm(popup.with_reason(reason)));
        Ok(())
    }

    /// `/roomnick`, `/topic` and `/roomname`: sent as from room info, where the room's
    /// name and topic can only be changed with the right to.
    fn room_text_command(&mut self, typed: &Typed<'_>) -> Result<(), String> {
        let room_id = self.open_room_or_say()?;
        if self.matrix.is_none() {
            return Err(NO_ACCOUNT.into());
        }
        let power = &self.room().ok_or("No room is open")?.power;
        let row = match typed.command {
            RoomCommand::Topic if !power.can_set_topic => {
                return Err("You are not allowed to change the topic here".into());
            }
            RoomCommand::RoomName if !power.can_rename => {
                return Err("You are not allowed to rename this room".into());
            }
            RoomCommand::Topic => InfoRow::Topic,
            RoomCommand::RoomName => InfoRow::Name,
            _ => InfoRow::OwnName,
        };
        let text = clean_input(typed.arg);
        if text.is_empty() {
            return Err(typed.command.usage(typed.name));
        }
        let (command, _) = room_text_change(row, room_id, text)?;
        // The answer comes as a toast, as it does when room info is closed meanwhile.
        self.manage(command);
        Ok(())
    }

    /// The open room's ID, or why there is nothing to act on.
    fn open_room_or_say(&self) -> Result<String, String> {
        let room = self.room().ok_or_else(|| "No room is open".to_owned())?;
        if room.is_pending() {
            return Err(room.kind.pending_hint().to_owned());
        }
        Ok(room.id.clone())
    }

    /// The person the command's argument names, and what follows.
    fn named<'a>(&self, typed: &Typed<'a>) -> Result<(Named, &'a str), String> {
        let members = self.known_members();
        resolve_user(
            typed.arg,
            typed.arg_start,
            typed.command.takes_reason(),
            &members,
            &self.complete.marks,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(user_id: &str, name: &str) -> Member {
        Member {
            user_id: user_id.into(),
            display_name: name.into(),
        }
    }

    fn members() -> Vec<Member> {
        vec![
            member("@alice:example.org", "Alice"),
            member("@bob:example.org", "Bob Builder"),
            member("@bobby:example.org", "Bob"),
            member("@carol:example.org", "carol"),
            member("@carol:other.org", "Carol"),
            member("@dan:example.org", "Dan the Man"),
        ]
    }

    fn who(arg: &str, with_rest: bool) -> Result<(String, String), String> {
        resolve_user(arg, 0, with_rest, &members(), &[])
            .map(|(named, rest)| (named.user_id, rest.to_owned()))
    }

    #[test]
    fn commands_are_known_by_name_with_what_follows() {
        let typed = parse("  /kick  @bob:example.org  spam  ").unwrap();
        assert_eq!(typed.command, RoomCommand::Kick);
        assert_eq!(typed.arg, "@bob:example.org  spam");
        assert_eq!(typed.arg_start, 9);
        assert_eq!(parse("/msg bob").unwrap().command, RoomCommand::DirectChat);
        assert_eq!(parse("/msg bob").unwrap().name, "msg");
        assert_eq!(parse("/leave").unwrap().arg, "");
        assert_eq!(parse("/leave").unwrap().arg_start, 6);
        // Text commands, other words, escaped slashes and plain text are not room commands.
        for text in [
            "/me waves",
            "/kicks x",
            "//kick bob",
            "kick bob",
            "/",
            "hi /kick",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
    }

    #[test]
    fn a_missing_argument_says_how_the_command_is_used() {
        assert_eq!(
            RoomCommand::Kick.usage("kick"),
            "/kick needs someone: /kick <user> [reason]"
        );
        assert_eq!(
            RoomCommand::DirectChat.usage("msg"),
            "/msg needs someone: /msg <user>"
        );
        assert!(RoomCommand::Leave.usage("leave").contains("open room"));
    }

    #[test]
    fn a_person_is_named_by_id_by_name_or_by_the_part_before_the_colon() {
        assert_eq!(
            who("@dave:example.org", false),
            Ok(("@dave:example.org".into(), String::new()))
        );
        assert_eq!(
            who("https://matrix.to/#/@dave:example.org", false),
            Ok(("@dave:example.org".into(), String::new()))
        );
        assert_eq!(
            who("alice", false),
            Ok(("@alice:example.org".into(), String::new()))
        );
        assert_eq!(
            who("@alice", false),
            Ok(("@alice:example.org".into(), String::new()))
        );
        assert_eq!(
            who("Bob Builder", false),
            Ok(("@bob:example.org".into(), String::new()))
        );
        // `bobby` by name and by the part before the colon is one person.
        assert_eq!(
            who("bobby", false),
            Ok(("@bobby:example.org".into(), String::new()))
        );
    }

    #[test]
    fn a_reason_may_follow_the_person() {
        assert_eq!(
            who("@dave:example.org being rude", true),
            Ok(("@dave:example.org".into(), "being rude".into()))
        );
        assert_eq!(
            who("Alice  spam", true),
            Ok(("@alice:example.org".into(), "spam".into()))
        );
        // `Dan` and `Dan the Man` are the same person; the longer name is the one meant.
        assert_eq!(
            who("Dan the Man spam", true),
            Ok(("@dan:example.org".into(), "spam".into()))
        );
        assert_eq!(
            who("alice", true),
            Ok(("@alice:example.org".into(), String::new()))
        );
        // Without room for a reason, the words are all one name.
        assert!(who("alice spam", false).is_err());
        assert!(who("@dave:example.org spam", false).is_err());
    }

    #[test]
    fn a_name_that_fits_several_people_or_nobody_is_refused() {
        // `bob` is the part before the colon of one and the name of another.
        let error = who("bob", false).unwrap_err();
        assert!(error.contains("More than one"), "{error}");
        // Names differ only in case; both fit.
        assert!(who("Carol", false).unwrap_err().contains("More than one"));
        // "Bob Builder spam": `Bob` is one person, `Bob Builder` another.
        assert!(who("Bob Builder spam", true).is_err());
        let error = who("zed", false).unwrap_err();
        assert!(error.contains("No one in this room") && error.contains("“zed”"));
        assert!(who("@not an id", false).is_err());
    }

    #[test]
    fn an_accepted_mention_names_the_person_it_stands_for() {
        let marks = [MentionMark::user("Bob", "@bobby:example.org").with_at(Some(6))];
        let (named, rest) =
            resolve_user("Bob too loud", 6, true, &members(), &marks).expect("named");
        assert_eq!(named.user_id, "@bobby:example.org");
        assert_eq!(rest, "too loud");
        // A mention somewhere else in the text is not the one named.
        let elsewhere = [MentionMark::user("Bob", "@bobby:example.org").with_at(Some(20))];
        assert!(resolve_user("Bob too loud", 6, true, &members(), &elsewhere).is_err());
    }

    /// Every command the app knows is offered when `/` is typed, and the other way
    /// round.
    #[test]
    fn every_room_command_is_offered_in_the_completions() {
        let names = [
            "join", "leave", "invite", "kick", "ban", "unban", "ignore", "unignore", "nick",
            "roomnick", "topic", "roomname", "dm", "msg", "whois", "help",
        ];
        for name in names {
            assert!(parse(&format!("/{name}")).is_some(), "{name}");
            assert!(
                crate::complete::COMMANDS
                    .iter()
                    .any(|(command, _)| *command == format!("/{name}")),
                "{name}"
            );
        }
    }

    mod in_the_app {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use tokio::sync::mpsc::UnboundedReceiver;

        use crate::action::{Action, Pane};
        use crate::app::App;
        use crate::complete::Member;
        use crate::config::Config;
        use crate::manage::{Confirm, InfoRow, Stage};
        use crate::matrix::{
            ManageCmd, MatrixCmd, MatrixEvent, MatrixHandle, RoomCategory, RoomSummary,
        };
        use crate::modal::{Modal, ReasonConfirmKind};
        use crate::model::RoomPower;
        use crate::people::{UserAction, UserInfoRow};

        const ROOM: &str = "!rats:example.org";
        const ME: &str = "@me:example.org";
        const BOB: &str = "@bob:example.org";

        fn summary(id: &str, name: &str, alias: Option<&str>) -> RoomSummary {
            RoomSummary {
                encrypted: false,
                id: id.into(),
                name: name.into(),
                category: RoomCategory::Room,
                notifications: 0,
                server_notifications: 0,
                mention: false,
                unread: false,
                marked_unread: false,
                preview: None,
                avatar: None,
                alias: alias.map(str::to_owned),
                direct: false,
                muted: false,
            }
        }

        fn power(moderator: bool) -> RoomPower {
            let mut power = RoomPower {
                own_level: if moderator { 50 } else { 0 },
                can_ban: moderator,
                can_unban: moderator,
                can_kick: moderator,
                can_rename: moderator,
                can_set_topic: moderator,
                ..RoomPower::default()
            };
            power.member_levels.insert(ME.into(), power.own_level);
            power
        }

        /// An app with an account, two rooms (the first open), bob and carol in it,
        /// and the commands it sends.
        fn connected(moderator: bool) -> (App, UnboundedReceiver<MatrixCmd>) {
            let (handle, mut commands) = MatrixHandle::for_test();
            let mut app = App::new(&Config::default(), std::path::PathBuf::new(), Some(handle));
            app.on_matrix(MatrixEvent::LoggedIn { user_id: ME.into() });
            app.on_matrix(MatrixEvent::Rooms(vec![
                summary(ROOM, "Rats", Some("#rats:example.org")),
                summary("!mice:example.org", "Mice", Some("#mice:example.org")),
            ]));
            app.on_matrix(MatrixEvent::RoomPower {
                room_id: ROOM.into(),
                power: power(moderator),
            });
            app.on_matrix(MatrixEvent::RoomMembers {
                room_id: ROOM.into(),
                members: vec![
                    Member {
                        user_id: BOB.into(),
                        display_name: "Bob".into(),
                    },
                    Member {
                        user_id: "@carol:example.org".into(),
                        display_name: "Carol".into(),
                    },
                ],
                can_mention_room: false,
            });
            while commands.try_recv().is_ok() {}
            (app, commands)
        }

        /// Types `text` into the composer and presses Enter.
        fn run(app: &mut App, text: &str) {
            app.perform(Action::Focus(Pane::Composer));
            app.composer.set_text(text);
            app.perform(Action::Send);
        }

        /// What was asked of the server since the last call (not drafts, typing and
        /// the like).
        fn sent(commands: &mut UnboundedReceiver<MatrixCmd>) -> Vec<MatrixCmd> {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter(|command| {
                    matches!(
                        command,
                        MatrixCmd::Manage(_)
                            | MatrixCmd::SetOwnDisplayName { .. }
                            | MatrixCmd::IgnoreUser { .. }
                            | MatrixCmd::UnignoreUser { .. }
                    )
                })
                .collect()
        }

        fn toast(app: &App) -> &str {
            app.toast.as_ref().map_or("", |(text, _)| text.as_str())
        }

        fn key(code: KeyCode) -> Event {
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
        }

        #[test]
        fn kick_ban_and_unban_ask_first_with_the_reason_typed_after_the_name() {
            let (mut app, mut commands) = connected(true);
            run(&mut app, "/kick bob being rude");
            let Some(Modal::ReasonConfirm(popup)) = &app.modal else {
                panic!("no question: {:?}", app.modal);
            };
            assert!(
                matches!(&popup.kind, ReasonConfirmKind::Kick { user_id, .. } if user_id == BOB)
            );
            assert_eq!(popup.reason.as_str(), "being rude");
            assert!(app.composer.is_empty());
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|f| crate::app::ui::draw(f, &mut app))
                .unwrap();
            let text = format!("{}", terminal.backend());
            assert!(text.contains("Remove this member from the room?"), "{text}");
            assert!(text.contains("Bob (@bob:example.org)"), "{text}");
            assert!(text.contains("[Remove]"), "{text}");
            assert!(sent(&mut commands).is_empty());
            app.on_event(key(KeyCode::Enter));
            assert!(matches!(
                sent(&mut commands).as_slice(),
                [MatrixCmd::Manage(ManageCmd::Kick { room_id, user_id, reason: Some(reason) })]
                    if room_id == ROOM && user_id == BOB && reason == "being rude"
            ));

            // Esc takes the question back; nothing is sent.
            run(&mut app, "/ban @dave:example.org");
            assert!(matches!(
                &app.modal,
                Some(Modal::ReasonConfirm(popup)) if matches!(&popup.kind, ReasonConfirmKind::Ban { user_id, .. } if user_id == "@dave:example.org")
            ));
            app.on_event(key(KeyCode::Esc));
            assert!(app.modal.is_none());
            run(&mut app, "/unban Carol");
            app.on_event(key(KeyCode::Enter));
            assert!(matches!(
                sent(&mut commands).as_slice(),
                [MatrixCmd::Manage(ManageCmd::Unban { user_id, reason: None, .. })]
                    if user_id == "@carol:example.org"
            ));

            // Oneself, and a missing name, are refused with the draft kept.
            run(&mut app, "/kick @me:example.org");
            assert_eq!(toast(&app), "To go yourself, use /leave");
            assert_eq!(app.composer.text(), "/kick @me:example.org");
            run(&mut app, "/ban ");
            assert_eq!(toast(&app), "/ban needs someone: /ban <user> [reason]");
            assert!(app.modal.is_none());
        }

        #[test]
        fn without_the_right_to_moderate_nothing_is_asked() {
            let (mut app, mut commands) = connected(false);
            run(&mut app, "/kick bob");
            assert_eq!(toast(&app), "You are not allowed to remove Bob here");
            run(&mut app, "/ban bob");
            assert_eq!(toast(&app), "You are not allowed to ban Bob here");
            run(&mut app, "/topic New topic");
            assert_eq!(toast(&app), "You are not allowed to change the topic here");
            run(&mut app, "/roomname Rodents");
            assert_eq!(toast(&app), "You are not allowed to rename this room");
            assert!(app.modal.is_none());
            assert_eq!(app.composer.text(), "/roomname Rodents");
            assert!(sent(&mut commands).is_empty());
        }

        #[test]
        fn an_accepted_mention_names_the_person_to_kick() {
            let (mut app, mut commands) = connected(true);
            app.perform(Action::Focus(Pane::Composer));
            for c in "/kick @bo".chars() {
                app.on_event(key(KeyCode::Char(c)));
            }
            app.on_event(key(KeyCode::Tab));
            assert_eq!(app.composer.text(), "/kick Bob ");
            for c in "spam".chars() {
                app.on_event(key(KeyCode::Char(c)));
            }
            app.on_event(key(KeyCode::Enter));
            app.on_event(key(KeyCode::Enter));
            assert!(matches!(
                sent(&mut commands).as_slice(),
                [.., MatrixCmd::Manage(ManageCmd::Kick { user_id, reason: Some(reason), .. })]
                    if user_id == BOB && reason == "spam"
            ));
        }

        #[test]
        fn leave_asks_in_room_info_and_takes_nothing_after_it() {
            let (mut app, mut commands) = connected(false);
            run(&mut app, "/leave #mice:example.org");
            assert!(toast(&app).starts_with("/leave takes nothing after it"));
            assert!(app.modal.is_none());
            run(&mut app, "/leave");
            let Some(Modal::RoomInfo(info)) = &app.modal else {
                panic!("room info is not open");
            };
            assert_eq!(info.room_id, ROOM);
            assert_eq!(info.current(), Some(InfoRow::Leave));
            assert_eq!(info.confirm, Some(Confirm::Leave));
            sent(&mut commands);
            app.on_event(key(KeyCode::Enter));
            assert!(sent(&mut commands).iter().any(|command| matches!(
                command,
                MatrixCmd::Manage(ManageCmd::Leave { room_id }) if room_id == ROOM
            )));
        }

        #[test]
        fn join_opens_a_joined_room_or_shows_the_preview() {
            let (mut app, mut commands) = connected(false);
            run(&mut app, "/join #mice:example.org");
            assert_eq!(app.room().map(|room| room.name.as_str()), Some("Mice"));
            assert!(app.modal.is_none());
            run(&mut app, "/join https://matrix.to/#/#cheese:example.org");
            let Some(Modal::New(popup)) = &app.modal else {
                panic!("no preview");
            };
            assert_eq!(popup.stage, Stage::Preview);
            assert!(sent(&mut commands).iter().any(|command| matches!(
                command,
                MatrixCmd::Manage(ManageCmd::Preview { target, .. }) if target == "#cheese:example.org"
            )));
            app.modal = None;
            run(&mut app, "/join @bob:example.org");
            assert_eq!(
                toast(&app),
                "That is a person, not a room: /dm opens a chat"
            );
            run(&mut app, "/join nowhere");
            assert!(app.modal.is_none());
            assert_eq!(app.composer.text(), "/join nowhere");
        }

        #[test]
        fn invite_and_direct_chats_go_through_their_forms() {
            let (mut app, mut commands) = connected(false);
            run(&mut app, "/invite @dave:example.org");
            assert!(sent(&mut commands).iter().any(|command| matches!(
                command,
                MatrixCmd::Manage(ManageCmd::Invite { room_id, user_id })
                    if room_id == ROOM && user_id == "@dave:example.org"
            )));
            assert!(matches!(&app.modal, Some(Modal::New(popup)) if popup.busy.is_some()));
            app.modal = None;
            // Typed on purpose, like in the form: a chat is made if there is none.
            run(&mut app, "/msg bob");
            assert!(sent(&mut commands).iter().any(|command| matches!(
                command,
                MatrixCmd::Manage(ManageCmd::DirectChat { user_id, create: true, .. })
                    if user_id == BOB
            )));
            app.modal = None;
            run(&mut app, "/dm someone");
            assert!(toast(&app).starts_with("No one in this room is called “someone”"));
        }

        #[test]
        fn ignore_asks_in_user_info_and_whois_shows_it() {
            let (mut app, mut commands) = connected(false);
            run(&mut app, "/unignore bob");
            assert_eq!(toast(&app), "Bob is not ignored");
            run(&mut app, "/ignore bob");
            let ctx = app.user_info_ctx(BOB);
            let Some(Modal::UserInfo(popup)) = &app.modal else {
                panic!("user info is not open");
            };
            assert_eq!(popup.user_id, BOB);
            assert_eq!(
                popup.current(&ctx),
                Some(UserInfoRow::Action(UserAction::IgnoreToggle))
            );
            assert_eq!(popup.confirm, Some(Confirm::Ignore(BOB.into())));
            app.on_event(key(KeyCode::Enter));
            assert!(sent(&mut commands).iter().any(
                |command| matches!(command, MatrixCmd::IgnoreUser { user_id } if user_id == BOB)
            ));
            app.modal = None;
            run(&mut app, "/ignore @me:example.org");
            assert_eq!(toast(&app), "You cannot ignore your own account");
            run(&mut app, "/whois carol");
            assert!(matches!(
                &app.modal,
                Some(Modal::UserInfo(popup)) if popup.user_id == "@carol:example.org" && popup.confirm.is_none()
            ));
        }

        #[test]
        fn names_and_the_topic_are_sent_as_from_their_forms() {
            let (mut app, mut commands) = connected(true);
            run(&mut app, "/nick  Ratty McRat ");
            run(&mut app, "/roomnick Ratty");
            run(&mut app, "/topic All about rats");
            run(&mut app, "/roomname Rodents");
            let sent = sent(&mut commands);
            assert!(
                matches!(
                    sent.as_slice(),
                    [
                        MatrixCmd::SetOwnDisplayName { name: Some(nick) },
                        MatrixCmd::Manage(ManageCmd::SetOwnName { name: Some(room_nick), .. }),
                        MatrixCmd::Manage(ManageCmd::SetTopic { topic, .. }),
                        MatrixCmd::Manage(ManageCmd::SetName { name, .. }),
                    ] if nick == "Ratty McRat" && room_nick == "Ratty" && topic == "All about rats" && name == "Rodents"
                ),
                "{sent:?}"
            );
            assert!(app.composer.is_empty());
            run(&mut app, "/nick");
            assert_eq!(toast(&app), "/nick needs a name: /nick <display name>");
        }

        #[test]
        fn help_opens_and_unknown_commands_are_refused_as_before() {
            let (mut app, mut commands) = connected(false);
            run(&mut app, "/help");
            assert!(app.help_open);
            app.help_open = false;
            run(&mut app, "/frobnicate now");
            assert_eq!(
                toast(&app),
                "Unknown command /frobnicate. Start with // to send it as text."
            );
            assert_eq!(app.composer.text(), "/frobnicate now");
            assert!(sent(&mut commands).is_empty());
        }

        #[test]
        fn a_command_waits_while_a_message_is_edited() {
            let (mut app, mut commands) = connected(true);
            app.compose_mode = crate::app::ComposeMode::Edit(0);
            run(&mut app, "/kick bob");
            assert_eq!(toast(&app), "Finish or cancel the edit first");
            assert!(app.modal.is_none());
            assert!(sent(&mut commands).is_empty());
        }

        #[test]
        fn the_demo_explains_that_it_has_no_account() {
            let mut app = App::new(&Config::default(), std::path::PathBuf::new(), None);
            run(&mut app, "/topic Hello");
            assert_eq!(toast(&app), super::super::NO_ACCOUNT);
            run(&mut app, "/whois @someone:example.org");
            assert!(matches!(&app.modal, Some(Modal::UserInfo(_))));
        }
    }

    #[test]
    fn debug_output_does_not_name_the_person() {
        let named = Named {
            user_id: "@secret:example.org".into(),
            name: Some("Secret Name".into()),
        };
        let typed = parse("/kick @secret:example.org secret reason");
        let debug = format!("{named:?} {typed:?}");
        assert!(
            !debug.contains("secret") && !debug.contains("Secret"),
            "{debug}"
        );
    }
}
