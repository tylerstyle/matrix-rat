//! State and redacting Debug implementations for User Info and Own Profile popups.

use std::fmt;

use crate::lineedit::LineEdit;
use crate::manage::{BanPrompt, Confirm, FIELD_MAX, RoleChooser};
use crate::matrix::{
    Change, OwnProfile, OwnProfileAction, RoomMember, UserProfile, UserSharedRoom,
    UserVerifyStatus, fallback_localpart,
};
use crate::text::is_blank;

/// Context about the app session that controls which User Info rows are shown and enabled.
#[derive(Debug, Clone, Copy, Default)]
pub struct UserInfoCtx {
    pub connected: bool,
    pub can_mention: bool,
    pub is_self: bool,
    pub ignored: bool,
}

/// Interactive actions available inside the User Info popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAction {
    DirectChat,
    Mention,
    Verify,
    IgnoreToggle,
    Role,
    Kick,
    Ban,
    Unban,
}

impl UserAction {
    pub fn label(self, ignored: bool) -> &'static str {
        match self {
            Self::DirectChat => "Direct chat",
            Self::Mention => "Mention in composer",
            Self::Verify => "Verify user",
            Self::IgnoreToggle => {
                if ignored {
                    "Stop ignoring"
                } else {
                    "Ignore user"
                }
            }
            Self::Role => "Change role…",
            Self::Kick => "Remove from room",
            Self::Ban => "Ban from room…",
            Self::Unban => "Unban from room",
        }
    }

    pub fn is_destructive(self) -> bool {
        matches!(self, Self::Kick | Self::Ban)
    }
}

/// One selectable row in the User Info popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserInfoRow {
    Action(UserAction),
    SharedRoom(usize),
}

/// Pending worker operation owned by the open User Info popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPending {
    Chat(u64),
    Manage(Change),
    Ignore(bool),
}

/// Modal showing another user's (or own) profile, shared rooms, and contextual actions.
#[derive(Clone)]
pub struct UserInfoPopup {
    pub user_id: String,
    pub room_id: Option<String>,
    pub initial_name: Option<String>,
    pub initial_member: Option<RoomMember>,
    pub own_is_creator: bool,
    pub own_power_level: i64,
    pub admin_count: usize,
    pub loading: bool,
    pub request: u64,
    pub profile: Option<UserProfile>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub selected: usize,
    pub scroll: usize,
    pub page: usize,
    pub reveal: bool,
    pub confirm: Option<Confirm>,
    pub ban_prompt: Option<BanPrompt>,
    pub role_chooser: Option<RoleChooser>,
    pub busy: Option<&'static str>,
    pub pending: Option<UserPending>,
}

/// Never logs user IDs, names, error text, or ban reasons.
impl fmt::Debug for UserInfoPopup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserInfoPopup")
            .field("loading", &self.loading)
            .field("has_profile", &self.profile.is_some())
            .field("has_room", &self.room_id.is_some())
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

impl UserInfoPopup {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: String,
        room_id: Option<String>,
        initial_name: Option<String>,
        initial_member: Option<RoomMember>,
        own_is_creator: bool,
        own_power_level: i64,
        admin_count: usize,
        request: u64,
        loading: bool,
    ) -> Self {
        Self {
            user_id,
            room_id,
            initial_name,
            initial_member,
            own_is_creator,
            own_power_level,
            admin_count,
            loading,
            request,
            profile: None,
            error: None,
            notice: None,
            selected: 0,
            scroll: 0,
            page: 10,
            reveal: true,
            confirm: None,
            ban_prompt: None,
            role_chooser: None,
            busy: None,
            pending: None,
        }
    }

    pub fn wait_for(&mut self, pending: UserPending, label: &'static str) {
        self.busy = Some(label);
        self.pending = Some(pending);
    }

    pub fn answered(&mut self, answer: UserPending) -> bool {
        if self.pending != Some(answer) {
            return false;
        }
        self.pending = None;
        self.busy = None;
        true
    }

    /// Context-room membership details, preferring the refreshed remote snapshot.
    pub fn effective_member(&self) -> Option<&RoomMember> {
        self.profile
            .as_ref()
            .and_then(|p| p.room_member.as_ref())
            .or(self.initial_member.as_ref())
    }

    pub fn display_name(&self) -> String {
        if let Some(name) = self
            .profile
            .as_ref()
            .and_then(|p| p.display_name.as_deref())
            .filter(|n| !is_blank(n))
        {
            return name.to_owned();
        }
        if let Some(name) = self
            .initial_member
            .as_ref()
            .map(|m| m.name.as_str())
            .filter(|n| !is_blank(n))
        {
            return name.to_owned();
        }
        if let Some(name) = self.initial_name.as_deref().filter(|n| !is_blank(n)) {
            return name.to_owned();
        }
        fallback_localpart(&self.user_id)
    }

    pub fn avatar_url(&self) -> Option<&str> {
        self.profile.as_ref()?.avatar.as_deref()
    }

    pub fn verify_status(&self) -> UserVerifyStatus {
        self.profile
            .as_ref()
            .map_or(UserVerifyStatus::Unknown, |p| p.verify)
    }

    pub fn shared_rooms(&self) -> &[UserSharedRoom] {
        self.profile
            .as_ref()
            .map_or(&[], |p| p.shared_rooms.as_slice())
    }

    pub fn rows(&self, ctx: &UserInfoCtx) -> Vec<UserInfoRow> {
        let mut rows = Vec::new();
        if !ctx.is_self {
            rows.push(UserInfoRow::Action(UserAction::DirectChat));
        }
        if ctx.can_mention {
            rows.push(UserInfoRow::Action(UserAction::Mention));
        }
        if !ctx.is_self {
            if self.verify_status() != UserVerifyStatus::Verified {
                rows.push(UserInfoRow::Action(UserAction::Verify));
            }
            rows.push(UserInfoRow::Action(UserAction::IgnoreToggle));
        }
        if let Some(member) = self.effective_member() {
            if member.role_editable {
                rows.push(UserInfoRow::Action(UserAction::Role));
            }
            if member.removable {
                rows.push(UserInfoRow::Action(UserAction::Kick));
            }
            if member.bannable {
                rows.push(UserInfoRow::Action(UserAction::Ban));
            }
            if member.unbannable {
                rows.push(UserInfoRow::Action(UserAction::Unban));
            }
        }
        for index in 0..self.shared_rooms().len() {
            rows.push(UserInfoRow::SharedRoom(index));
        }
        rows
    }

    pub fn action_disabled_reason(
        &self,
        action: UserAction,
        ctx: &UserInfoCtx,
    ) -> Option<&'static str> {
        match action {
            UserAction::Mention => (!ctx.can_mention).then_some("Open a joined room to mention."),
            UserAction::DirectChat
            | UserAction::IgnoreToggle
            | UserAction::Role
            | UserAction::Kick
            | UserAction::Ban
            | UserAction::Unban => {
                (!ctx.connected).then_some("The demo has no account to do this with.")
            }
            UserAction::Verify => {
                if !ctx.connected {
                    Some("The demo has no account to do this with.")
                } else if self.loading {
                    Some("Loading encryption keys…")
                } else if self.verify_status() == UserVerifyStatus::Verified {
                    Some("This user is already verified.")
                } else if self.verify_status() == UserVerifyStatus::NoKeys {
                    Some("This user has no encryption keys yet.")
                } else {
                    None
                }
            }
        }
    }

    pub fn current(&self, ctx: &UserInfoCtx) -> Option<UserInfoRow> {
        let rows = self.rows(ctx);
        rows.get(self.selected.min(rows.len().saturating_sub(1)))
            .copied()
    }

    pub fn clear_subprompts(&mut self) {
        self.confirm = None;
        self.ban_prompt = None;
        self.role_chooser = None;
    }

    pub fn step(&mut self, delta: isize, ctx: &UserInfoCtx) {
        let count = self.rows(ctx).len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        self.clear_subprompts();
        let next = self
            .selected
            .saturating_add_signed(delta)
            .min(count.saturating_sub(1));
        if next != self.selected {
            self.selected = next;
            self.reveal = true;
        }
    }

    pub fn select(&mut self, index: usize, ctx: &UserInfoCtx) {
        let count = self.rows(ctx).len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let target = index.min(count - 1);
        if target != self.selected {
            self.clear_subprompts();
        }
        self.selected = target;
        self.reveal = true;
    }

    pub fn step_page(&mut self, delta: isize, ctx: &UserInfoCtx) {
        let by = (self.page.max(1) as isize - 1).max(1) * delta.signum();
        self.step(by, ctx);
    }

    pub fn open_ban_prompt(&mut self) {
        let name = self.display_name();
        self.clear_subprompts();
        self.notice = None;
        self.error = None;
        self.ban_prompt = Some(BanPrompt::new(self.user_id.clone(), name));
    }

    pub fn open_role_chooser(&mut self) {
        let Some(member) = self.effective_member().cloned() else {
            return;
        };
        let chooser = RoleChooser::new(
            &member,
            self.own_is_creator,
            self.own_power_level,
            self.admin_count,
        );
        self.clear_subprompts();
        self.notice = None;
        if chooser.options.is_empty() {
            self.error = Some("Your role does not allow changing this member's role.".to_owned());
            return;
        }
        self.error = None;
        self.role_chooser = Some(chooser);
    }

    pub fn confirm_prompt(&self) -> Option<String> {
        let name = self.display_name();
        Some(match self.confirm.as_ref()? {
            Confirm::NewChat(_) => format!(
                "Enter again to start one, Esc to stay · no chat with {name} yet; they get an invitation"
            ),
            Confirm::Remove(_) => {
                format!("Enter again to remove, Esc to keep · {name} will be out of the room")
            }
            Confirm::Unban(_) => {
                format!("Enter again to unban, Esc to keep · {name} will be allowed back")
            }
            Confirm::Ignore(_) => {
                format!("Enter again to ignore, Esc to stay · {name}'s messages will be hidden")
            }
            Confirm::Unignore(_) => format!(
                "Enter again to stop ignoring, Esc to stay · {name}'s messages will show again"
            ),
            Confirm::Leave => return None,
        })
    }
}

/// Form for viewing and changing the account's own display name and avatar.
#[derive(Clone)]
pub struct OwnProfilePopup {
    pub user_id: Option<String>,
    pub current_name: Option<String>,
    pub current_avatar: Option<String>,
    pub display_name: LineEdit,
    pub avatar_path: LineEdit,
    pub dirty_name: bool,
    pub focus: usize,
    pub busy: Option<&'static str>,
    pub pending: Option<OwnProfileAction>,
    pub error: Option<String>,
    pub notice: Option<String>,
}

/// Never prints display names, user IDs, file paths, or messages to logs.
impl fmt::Debug for OwnProfilePopup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnProfilePopup")
            .field("has_name", &self.current_name.is_some())
            .field("has_avatar", &self.current_avatar.is_some())
            .field("focus", &self.focus)
            .field("busy", &self.busy)
            .finish_non_exhaustive()
    }
}

impl OwnProfilePopup {
    pub const NAME_INPUT: usize = 0;
    pub const SAVE_NAME: usize = 1;
    pub const AVATAR_PATH: usize = 2;
    pub const SET_AVATAR: usize = 3;
    pub const REMOVE_AVATAR: usize = 4;

    pub fn new(user_id: Option<String>, profile: Option<&OwnProfile>) -> Self {
        let current_name = profile.and_then(|p| p.display_name.clone());
        let current_avatar = profile.and_then(|p| p.avatar.clone());
        let mut display_name = LineEdit::new().with_max_bytes(FIELD_MAX);
        if let Some(name) = &current_name {
            display_name.set(name);
        }
        Self {
            user_id,
            current_name,
            current_avatar,
            display_name,
            avatar_path: LineEdit::new().with_max_bytes(4096),
            dirty_name: false,
            focus: Self::NAME_INPUT,
            busy: None,
            pending: None,
            error: None,
            notice: None,
        }
    }

    pub fn slots(&self) -> &'static [usize] {
        if self.current_avatar.is_some() {
            &[
                Self::NAME_INPUT,
                Self::SAVE_NAME,
                Self::AVATAR_PATH,
                Self::SET_AVATAR,
                Self::REMOVE_AVATAR,
            ]
        } else {
            &[
                Self::NAME_INPUT,
                Self::SAVE_NAME,
                Self::AVATAR_PATH,
                Self::SET_AVATAR,
            ]
        }
    }

    pub fn step(&mut self, delta: isize) {
        let slots = self.slots();
        let pos = slots.iter().position(|s| *s == self.focus).unwrap_or(0);
        let next = (pos as isize + delta).rem_euclid(slots.len() as isize) as usize;
        self.focus = slots[next];
    }

    pub fn set_focus(&mut self, slot: usize) {
        if self.slots().contains(&slot) {
            self.focus = slot;
        }
    }

    pub fn sync_from(&mut self, profile: &OwnProfile) {
        self.current_name = profile.display_name.clone();
        self.current_avatar = profile.avatar.clone();
        if !self.dirty_name {
            self.display_name
                .set(profile.display_name.as_deref().unwrap_or(""));
        }
        if !self.slots().contains(&self.focus) {
            self.focus = Self::AVATAR_PATH;
        }
    }

    pub fn effective_name(&self) -> String {
        if let Some(name) = self.current_name.as_deref().filter(|n| !is_blank(n)) {
            return name.to_owned();
        }
        if let Some(uid) = &self.user_id {
            return fallback_localpart(uid);
        }
        "you".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matrix::MemberRole;

    fn sample_member(own: bool) -> RoomMember {
        RoomMember {
            user_id: if own {
                "@me:hs".into()
            } else {
                "@bob:hs".into()
            },
            name: if own { "Me".into() } else { "Bob".into() },
            role: MemberRole::User,
            power_level: 0,
            invited: false,
            banned: false,
            own,
            removable: !own,
            bannable: !own,
            unbannable: false,
            role_editable: true,
        }
    }

    #[test]
    fn user_info_popup_redacts_debug_output() {
        let mut popup = UserInfoPopup::new(
            "@secret_target:hs".into(),
            Some("!secret_room:hs".into()),
            Some("Secret Display Name".into()),
            None,
            false,
            0,
            1,
            7,
            false,
        );
        popup.error = Some("secret failure detail".into());
        popup.notice = Some("secret notice".into());
        popup.ban_prompt = Some(BanPrompt::new("@secret_target:hs".into(), "Secret".into()));
        if let Some(bp) = &mut popup.ban_prompt {
            bp.reason.set("top secret reason");
        }
        let dbg = format!("{popup:?}");
        assert!(!dbg.contains("secret_target"));
        assert!(!dbg.contains("secret_room"));
        assert!(!dbg.contains("Secret Display Name"));
        assert!(!dbg.contains("secret failure detail"));
        assert!(!dbg.contains("top secret reason"));
    }

    #[test]
    fn own_profile_popup_redacts_debug_and_skips_remove_when_no_avatar() {
        let profile = OwnProfile {
            display_name: Some("Alice Secret".into()),
            avatar: None,
            ..OwnProfile::default()
        };
        let mut popup = OwnProfilePopup::new(Some("@alice:hs".into()), Some(&profile));
        popup.avatar_path.set("/home/alice/private/pic.png");
        popup.error = Some("private upload err".into());
        let dbg = format!("{popup:?}");
        assert!(!dbg.contains("Alice Secret"));
        assert!(!dbg.contains("@alice:hs"));
        assert!(!dbg.contains("private/pic.png"));
        assert!(!dbg.contains("private upload err"));

        assert_eq!(popup.slots(), &[0, 1, 2, 3]);
        popup.focus = OwnProfilePopup::SET_AVATAR;
        popup.step(1);
        assert_eq!(popup.focus, OwnProfilePopup::NAME_INPUT);

        let with_avatar = OwnProfile {
            display_name: Some("Alice Updated".into()),
            avatar: Some("mxc://hs/av1".into()),
            ..OwnProfile::default()
        };
        popup.sync_from(&with_avatar);
        assert_eq!(popup.display_name.as_str(), "Alice Updated");
        assert_eq!(popup.slots(), &[0, 1, 2, 3, 4]);
    }

    #[test]
    fn user_info_rows_and_permissions_match_context() {
        let popup = UserInfoPopup::new(
            "@bob:hs".into(),
            Some("!room:hs".into()),
            Some("Bob".into()),
            Some(sample_member(false)),
            false,
            100,
            2,
            1,
            false,
        );
        let ctx = UserInfoCtx {
            connected: true,
            can_mention: true,
            is_self: false,
            ignored: true,
        };
        let rows = popup.rows(&ctx);
        assert_eq!(
            rows,
            vec![
                UserInfoRow::Action(UserAction::DirectChat),
                UserInfoRow::Action(UserAction::Mention),
                UserInfoRow::Action(UserAction::Verify),
                UserInfoRow::Action(UserAction::IgnoreToggle),
                UserInfoRow::Action(UserAction::Role),
                UserInfoRow::Action(UserAction::Kick),
                UserInfoRow::Action(UserAction::Ban),
            ]
        );
        assert_eq!(UserAction::IgnoreToggle.label(ctx.ignored), "Stop ignoring");

        let mut verified_popup = popup.clone();
        verified_popup.profile = Some(UserProfile {
            verify: UserVerifyStatus::Verified,
            ..UserProfile::default()
        });
        assert!(
            !verified_popup
                .rows(&ctx)
                .contains(&UserInfoRow::Action(UserAction::Verify))
        );
        assert!(
            verified_popup
                .action_disabled_reason(UserAction::Verify, &ctx)
                .is_some()
        );

        let self_popup = UserInfoPopup::new(
            "@me:hs".into(),
            Some("!room:hs".into()),
            Some("Me".into()),
            Some(sample_member(true)),
            false,
            100,
            1,
            2,
            false,
        );
        let self_ctx = UserInfoCtx {
            connected: true,
            can_mention: false,
            is_self: true,
            ignored: false,
        };
        assert_eq!(
            self_popup.rows(&self_ctx),
            vec![UserInfoRow::Action(UserAction::Role)]
        );
    }
}
