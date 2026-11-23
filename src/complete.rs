//! Composer completions (`@` mentions, `:emoji:`, `#` rooms, `/` commands) and
//! resolution of accepted mentions and room links into Matrix HTML + `m.mentions`.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ops::Range;

use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::{OwnedUserId, RoomAliasId, RoomId, UserId};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::matrix::EmoteCatalog;
use crate::text::sanitize_line;

/// Maximum number of completion candidates retained and displayed in the popup (slash
/// commands are all kept).
pub const MAX_CANDIDATES: usize = 10;

/// What kind of completion trigger is active at the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Mention,
    Emoji,
    Room,
    Command,
}

/// One room member eligible for `@` completion.
#[derive(Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: String,
    pub display_name: String,
}

/// Hand-written: who it is stays out of logs.
impl std::fmt::Debug for Member {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Member")
    }
}

/// One joined room eligible for `#` completion.
#[derive(Clone, PartialEq, Eq)]
pub struct RoomTarget {
    pub id: String,
    pub name: String,
    pub alias: Option<String>,
}

/// Hand-written: which room, its name and its address stay out of logs.
impl std::fmt::Debug for RoomTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomTarget")
            .field("alias", &self.alias.is_some())
            .finish_non_exhaustive()
    }
}

/// What happens when a completion item is accepted.
#[derive(Clone, PartialEq, Eq)]
pub enum Target {
    User {
        user_id: String,
        display_name: String,
    },
    RoomMention,
    Emoji(String),
    /// A custom emoji from an image pack, by its shortcode (without colons).
    CustomEmoji {
        shortcode: String,
        url: String,
    },
    RoomLink {
        label: String,
        uri: String,
    },
    Command(String),
}

/// Hand-written: names, ids, addresses and shortcodes stay out of logs.
impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::User { .. } => "Target::User",
            Self::RoomMention => "Target::RoomMention",
            Self::Emoji(_) => "Target::Emoji",
            Self::CustomEmoji { .. } => "Target::CustomEmoji",
            Self::RoomLink { .. } => "Target::RoomLink",
            Self::Command(_) => "Target::Command",
        })
    }
}

/// One row shown in the completion popup.
#[derive(Clone, PartialEq, Eq)]
pub struct Candidate {
    pub primary: String,
    pub secondary: String,
    pub target: Target,
}

/// Hand-written: what the row says names a person, a room or an emoji.
impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// What kind of target a [`MentionMark`] resolves to.
// Tests compare it with `assert_eq!`: there it shows everything, for a failure to be read.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, PartialEq, Eq)]
pub enum MentionKind {
    User {
        label: String,
        user_id: String,
    },
    RoomNotification,
    RoomLink {
        label: String,
        uri: String,
    },
    /// A custom emoji: `code` is its `:shortcode:` as typed, `url` its picture.
    Emoticon {
        code: String,
        url: String,
    },
}

/// Outside tests: names, ids, addresses and shortcodes stay out of logs.
#[cfg(not(test))]
impl std::fmt::Debug for MentionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::User { .. } => "MentionKind::User",
            Self::RoomNotification => "MentionKind::RoomNotification",
            Self::RoomLink { .. } => "MentionKind::RoomLink",
            Self::Emoticon { .. } => "MentionKind::Emoticon",
        })
    }
}

/// An accepted mention or room link tracked alongside composer text.
// Tests compare it with `assert_eq!`: there it shows everything, for a failure to be read.
#[cfg_attr(test, derive(Debug))]
#[derive(Clone, PartialEq, Eq)]
pub struct MentionMark {
    pub kind: MentionKind,
    /// Byte offset in composer text where this completion was accepted, if known.
    pub at: Option<usize>,
}

/// Outside tests: what it names stays out of logs.
#[cfg(not(test))]
impl std::fmt::Debug for MentionMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MentionMark")
            .field("kind", &self.kind)
            .field("at", &self.at)
            .finish()
    }
}

impl MentionMark {
    pub fn user(label: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            kind: MentionKind::User {
                label: label.into(),
                user_id: user_id.into(),
            },
            at: None,
        }
    }

    pub fn room_notification() -> Self {
        Self {
            kind: MentionKind::RoomNotification,
            at: None,
        }
    }

    pub fn room_link(label: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            kind: MentionKind::RoomLink {
                label: label.into(),
                uri: uri.into(),
            },
            at: None,
        }
    }

    pub fn emoticon(shortcode: &str, url: impl Into<String>) -> Self {
        Self {
            kind: MentionKind::Emoticon {
                code: format!(":{shortcode}:"),
                url: url.into(),
            },
            at: None,
        }
    }

    pub fn with_at(mut self, at: Option<usize>) -> Self {
        self.at = at;
        self
    }

    pub fn needle(&self) -> &str {
        match &self.kind {
            MentionKind::User { label, .. } | MentionKind::RoomLink { label, .. } => label.as_str(),
            MentionKind::Emoticon { code, .. } => code.as_str(),
            MentionKind::RoomNotification => "@room",
        }
    }

    /// Returns a copy whose anchor has been shifted left by `delta` bytes (or cleared
    /// if `delta` moves past the anchor).
    pub fn shifted(&self, delta: usize) -> Self {
        let mut copy = self.clone();
        copy.at = self.at.and_then(|pos| pos.checked_sub(delta));
        copy
    }

    /// Updates the stored byte anchor after replacing `edit_start..old_end` with `new_end`.
    pub fn adjust_for_edit(&mut self, edit_start: usize, old_end: usize, new_end: usize) {
        let Some(start) = self.at else {
            return;
        };
        let end = start + self.needle().len();
        if old_end <= start {
            let delta = new_end as isize - old_end as isize;
            self.at = Some((start as isize + delta).max(0) as usize);
        } else if edit_start < end {
            // Edit overlapped the accepted label span itself; fall back to first unused match.
            self.at = None;
        }
    }
}

/// Active completion state while the user is typing in the composer.
#[derive(Clone, PartialEq, Eq)]
pub struct Completion {
    pub kind: Kind,
    /// Byte range in composer text covered by the trigger and query (e.g. `@al`).
    pub range: Range<usize>,
    pub items: Vec<Candidate>,
    pub selected: usize,
}

/// Hand-written: the rows name people, rooms and emoji (and their `Debug`s keep that out).
impl std::fmt::Debug for Completion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Completion")
            .field("kind", &self.kind)
            .field("range", &self.range)
            .field("items", &self.items)
            .field("selected", &self.selected)
            .finish()
    }
}

/// Inputs used to compute completion candidates at the current cursor.
pub struct Context<'a> {
    pub members: &'a [Member],
    /// Most-recent-first unique speaker names in the current timeline.
    pub recent_speakers: &'a [String],
    pub can_mention_room: bool,
    pub rooms: &'a [RoomTarget],
    /// Custom emoji offered next to the standard ones.
    pub emotes: &'a EmoteCatalog,
}

/// Slash commands recognised by `crate::format`, `crate::upload` and the app (polls,
/// stickers and the room commands of `app::commands`).
pub(crate) const COMMANDS: &[(&str, &str)] = &[
    ("/me", "Send an action message (* you …)"),
    ("/plain", "Send without Markdown formatting"),
    ("/html", "Send raw HTML formatting"),
    ("/spoiler", "Hide text behind a spoiler"),
    ("/rainbow", "Colour each character of the message"),
    ("/shrug", "Prepend ¯\\_(ツ)_/¯ to the message"),
    ("/tableflip", "Prepend (╯°□°)╯︵ ┻━┻ to the message"),
    ("/lenny", "Prepend ( ͡° ͜ʖ ͡°) to the message"),
    ("/code", "Send as a code block: /code [language] text"),
    ("/poll", "Start a poll"),
    ("/sticker", "Send a sticker from your image packs"),
    ("/upload", "Attach and upload a file"),
    ("/file", "Attach and upload a file"),
    ("/join", "Join or open a room by address or link"),
    ("/leave", "Leave this room"),
    ("/invite", "Invite someone to this room"),
    ("/kick", "Remove someone from this room"),
    ("/ban", "Ban someone from this room"),
    ("/unban", "Let a banned person back in"),
    ("/ignore", "Hide someone's messages"),
    ("/unignore", "Stop ignoring someone"),
    ("/nick", "Change your display name"),
    ("/roomnick", "Change your name in this room"),
    ("/topic", "Change this room's topic"),
    ("/roomname", "Rename this room"),
    ("/dm", "Open a direct chat with someone"),
    ("/msg", "Open a direct chat with someone"),
    ("/whois", "Show someone's profile"),
    ("/archive", "Write this room's messages to a Markdown file"),
    ("/catchup", "Summarise unread, or the last N: /catchup [N]"),
    ("/help", "Show the keys and what they do"),
];

impl Completion {
    pub fn move_by(&mut self, delta: isize) {
        if self.items.is_empty() {
            self.selected = 0;
            return;
        }
        let len = self.items.len() as isize;
        self.selected = (self.selected as isize + delta).rem_euclid(len) as usize;
    }

    pub fn current(&self) -> Option<&Candidate> {
        self.items.get(self.selected)
    }
}

/// Text to insert when `target` is accepted for `text[range]` (taking surrounding text
/// into account for spacing), plus an optional mention/link mark to remember.
pub fn replacement(
    text: &str,
    range: &Range<usize>,
    target: &Target,
) -> (String, Option<MentionMark>) {
    let suffix = after_suffix(text.get(range.end..).unwrap_or(""));
    match target {
        Target::User {
            user_id,
            display_name,
        } => (
            format!("{display_name}{suffix}"),
            Some(
                MentionMark::user(display_name.clone(), user_id.clone()).with_at(Some(range.start)),
            ),
        ),
        Target::RoomMention => (
            format!("@room{suffix}"),
            Some(MentionMark::room_notification().with_at(Some(range.start))),
        ),
        Target::Emoji(ch) => (ch.clone(), None),
        // Spaced like a mention: a shortcode only becomes a picture when it stands apart.
        Target::CustomEmoji { shortcode, url } => (
            format!(":{shortcode}:{suffix}"),
            Some(MentionMark::emoticon(shortcode, url.clone()).with_at(Some(range.start))),
        ),
        Target::RoomLink { label, uri } => (
            format!("{label}{suffix}"),
            Some(MentionMark::room_link(label.clone(), uri.clone()).with_at(Some(range.start))),
        ),
        Target::Command(cmd) => {
            let needs_space = !text
                .get(range.end..)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace));
            let mut out = cmd.clone();
            if needs_space {
                out.push(' ');
            }
            (out, None)
        }
    }
}

fn after_suffix(rest: &str) -> &'static str {
    match rest.chars().next() {
        Some(ch)
            if ch.is_whitespace()
                || matches!(ch, ',' | '.' | ':' | ';' | '!' | '?' | ')' | ']') =>
        {
            ""
        }
        _ => " ",
    }
}

/// Finds the active completion trigger and ranked candidates for `text` at `cursor`.
pub fn find(text: &str, cursor: usize, ctx: &Context<'_>) -> Option<Completion> {
    let cursor = clamp_boundary(text, cursor);
    if cursor == 0 {
        return None;
    }
    let prefix = &text[..cursor];
    let mask = code_mask(prefix, true);
    if *mask.last()? {
        return None;
    }

    let (kind, range, query) = detect_trigger(prefix, &mask)?;
    let items = match kind {
        Kind::Command => match_commands(query),
        Kind::Emoji => match_emoji(query, ctx.emotes),
        Kind::Mention => match_mentions(query, ctx),
        Kind::Room => match_rooms(query, ctx.rooms),
    };
    if items.is_empty() {
        return None;
    }
    Some(Completion {
        kind,
        range,
        items,
        selected: 0,
    })
}

/// The kind of completion being typed at `cursor`, whether or not anything matches.
pub fn trigger(text: &str, cursor: usize) -> Option<Kind> {
    let cursor = clamp_boundary(text, cursor);
    let prefix = &text[..cursor];
    let mask = code_mask(prefix, true);
    if mask.last().is_none_or(|&code| code) {
        return None;
    }
    detect_trigger(prefix, &mask).map(|(kind, _, _)| kind)
}

fn clamp_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn detect_trigger<'a>(prefix: &'a str, mask: &[bool]) -> Option<(Kind, Range<usize>, &'a str)> {
    // `/commands`: strictly at the start of the message (single slash, no whitespace yet).
    if prefix.starts_with('/')
        && !prefix.starts_with("//")
        && !prefix[1..].contains(char::is_whitespace)
    {
        return Some((Kind::Command, 0..prefix.len(), &prefix[1..]));
    }

    // Scan the current whitespace-delimited token ending at the cursor.
    let token_start = prefix
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(i, ch)| i + ch.len_utf8());
    let token = &prefix[token_start..];
    if token.is_empty() {
        return None;
    }

    // Check triggers from closest to cursor backward inside the token.
    for (rel_idx, ch) in token.char_indices().rev() {
        let abs_idx = token_start + rel_idx;
        if mask.get(abs_idx).copied().unwrap_or(false) {
            continue;
        }
        let before = prefix[..abs_idx].chars().next_back();
        if !is_trigger_boundary(before) {
            continue;
        }
        let query = &prefix[abs_idx + ch.len_utf8()..];
        match ch {
            '@' => {
                return Some((Kind::Mention, abs_idx..prefix.len(), query));
            }
            '#' => {
                return Some((Kind::Room, abs_idx..prefix.len(), query));
            }
            // Emoji triggers require at least 2 query characters and must not look like URLs
            // or colons within words (already guarded by `is_trigger_boundary`, and reject
            // nested colons or scheme slashes inside the token).
            ':' if !query.contains(':')
                && !token[..rel_idx].contains("://")
                && query.chars().count() >= 2 =>
            {
                return Some((Kind::Emoji, abs_idx..prefix.len(), query));
            }
            _ => {}
        }
    }
    None
}

fn is_trigger_boundary(before: Option<char>) -> bool {
    match before {
        None => true,
        Some(ch) => {
            ch.is_whitespace() || matches!(ch, '(' | '[' | '{' | '<' | '"' | '\'' | ',' | ';')
        }
    }
}

/// All commands are offered (the popup scrolls): there are few, and a list that
/// stops short would hide some of them from someone looking for what there is.
fn match_commands(query: &str) -> Vec<Candidate> {
    let items: Vec<Candidate> = if query.is_empty() {
        COMMANDS
            .iter()
            .map(|&(cmd, desc)| command_candidate(cmd, desc))
            .collect()
    } else {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut buf = Vec::new();
        let lowered = query.to_lowercase();
        let mut scored: Vec<(u32, usize, &(&str, &str))> = COMMANDS
            .iter()
            .enumerate()
            .filter_map(|(idx, pair)| {
                let name = &pair.0[1..];
                let score = pattern.score(Utf32Str::new(name, &mut buf), &mut matcher)?;
                let boost = if name.starts_with(&lowered) {
                    10_000
                } else {
                    0
                };
                Some((boost + score, idx, pair))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .map(|(_, _, &(cmd, desc))| command_candidate(cmd, desc))
            .collect()
    };
    items
}

fn command_candidate(cmd: &str, desc: &str) -> Candidate {
    Candidate {
        primary: cmd.to_owned(),
        secondary: desc.to_owned(),
        target: Target::Command(cmd.to_owned()),
    }
}

/// The user's own custom emoji come first (those starting with the query before those
/// only containing it), then the standard ones.
fn match_emoji(query: &str, emotes: &EmoteCatalog) -> Vec<Candidate> {
    const MAX_CUSTOM: usize = 5;
    let needle = query.to_lowercase();
    let (mut starts, mut contains) = (Vec::new(), Vec::new());
    for emoji in emotes.emojis() {
        let code = emoji.shortcode.to_lowercase();
        if code.starts_with(&needle) {
            starts.push(emoji);
        } else if code.contains(&needle) {
            contains.push(emoji);
        }
    }
    let mut items: Vec<Candidate> = starts
        .into_iter()
        .chain(contains)
        .take(MAX_CUSTOM)
        .map(|emoji| Candidate {
            primary: format!(":{}:", emoji.shortcode),
            secondary: format!("custom · {}", emoji.pack_name),
            target: Target::CustomEmoji {
                shortcode: emoji.shortcode.clone(),
                url: emoji.url.clone(),
            },
        })
        .collect();
    items.extend(standard_emoji(query, MAX_CANDIDATES - items.len()));
    items
}

fn standard_emoji(query: &str, max: usize) -> impl Iterator<Item = Candidate> {
    crate::emoji::search(query)
        .into_iter()
        .take(max)
        .map(|emoji| {
            let short = emoji
                .shortcode()
                .map(|s| format!(":{s}:"))
                .unwrap_or_default();
            Candidate {
                primary: emoji.as_str().to_owned(),
                secondary: if short.is_empty() {
                    emoji.name().to_owned()
                } else {
                    format!("{short} — {}", emoji.name())
                },
                target: Target::Emoji(emoji.as_str().to_owned()),
            }
        })
}

enum MentionEntry<'a> {
    Member(&'a Member),
    Room,
}

impl MentionEntry<'_> {
    fn into_candidate(self) -> Candidate {
        match self {
            Self::Member(member) => member_candidate(member),
            Self::Room => room_mention_candidate(),
        }
    }
}

/// Sort key for a candidate name: pure ASCII stays borrowed and folds byte by byte
/// without allocating, while non-ASCII is lowercased once per query with
/// `str::to_lowercase`.
enum SortKey<'a> {
    Ascii(&'a str),
    Lowered(Cow<'a, str>),
}

impl<'a> SortKey<'a> {
    fn of(text: &'a str) -> Self {
        if text.is_ascii() {
            Self::Ascii(text)
        } else {
            Self::Lowered(Cow::Owned(text.to_lowercase()))
        }
    }

    fn from_lowered(text: &'a str, already_lowered: Option<String>) -> Self {
        match already_lowered {
            Some(lowered) => Self::Lowered(Cow::Owned(lowered)),
            None => Self::of(text),
        }
    }

    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Ascii(a), Self::Ascii(b)) => a
                .bytes()
                .map(|byte| byte.to_ascii_lowercase())
                .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase())),
            (Self::Ascii(a), Self::Lowered(b)) => a
                .bytes()
                .map(|byte| byte.to_ascii_lowercase())
                .cmp(b.bytes()),
            (Self::Lowered(a), Self::Ascii(b)) => a
                .bytes()
                .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase())),
            (Self::Lowered(a), Self::Lowered(b)) => a.as_ref().cmp(b.as_ref()),
        }
    }
}

fn match_mentions(query: &str, ctx: &Context<'_>) -> Vec<Candidate> {
    let recency = |name: &str, uid: &str| -> usize {
        ctx.recent_speakers
            .iter()
            .position(|s| s == name || s == uid)
            .unwrap_or(usize::MAX)
    };

    if query.is_empty() {
        let mut ranked: Vec<(usize, SortKey<'_>, MentionEntry<'_>)> =
            Vec::with_capacity(MAX_CANDIDATES);
        let cmp = |a: &(usize, SortKey<'_>, MentionEntry<'_>),
                   b: &(usize, SortKey<'_>, MentionEntry<'_>)| {
            a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1))
        };
        // Rank without allocating candidates or ASCII lowercase copies; build only the top 10.
        for member in ctx.members {
            let r = recency(&member.display_name, &member.user_id);
            if ranked.len() == MAX_CANDIDATES && r > ranked[MAX_CANDIDATES - 1].0 {
                continue;
            }
            push_top_k(
                &mut ranked,
                MAX_CANDIDATES,
                (
                    r,
                    SortKey::of(&member.display_name),
                    MentionEntry::Member(member),
                ),
                cmp,
            );
        }
        if ctx.can_mention_room {
            // Keep `@room` below regular members on an empty query so hitting Enter
            // accidentally after `@` does not ping the whole room.
            push_top_k(
                &mut ranked,
                MAX_CANDIDATES,
                (
                    usize::MAX,
                    SortKey::Lowered(Cow::Borrowed("\u{10ffff}room")),
                    MentionEntry::Room,
                ),
                cmp,
            );
        }
        return ranked
            .into_iter()
            .map(|(_, _, entry)| entry.into_candidate())
            .collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();
    let mut lower_buf = String::new();
    let lowered = query.to_lowercase();
    let mut top: Vec<(u32, usize, SortKey<'_>, MentionEntry<'_>)> =
        Vec::with_capacity(MAX_CANDIDATES);
    let cmp = |a: &(u32, usize, SortKey<'_>, MentionEntry<'_>),
               b: &(u32, usize, SortKey<'_>, MentionEntry<'_>)| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    };

    for member in ctx.members {
        let name_hit = score_field(
            &pattern,
            &mut matcher,
            &mut buf,
            &mut lower_buf,
            &member.display_name,
            &lowered,
        );
        let id_score = score_field(
            &pattern,
            &mut matcher,
            &mut buf,
            &mut lower_buf,
            &member.user_id,
            &lowered,
        )
        .map(|(score, _)| score);
        let (name_score, name_lowered) = match name_hit {
            Some((score, lowered_name)) => (Some(score), lowered_name),
            None => (None, None),
        };
        let Some(score) = name_score.into_iter().chain(id_score).max() else {
            continue;
        };
        if top.len() == MAX_CANDIDATES && score < top[MAX_CANDIDATES - 1].0 {
            continue;
        }
        let r = recency(&member.display_name, &member.user_id);
        if top.len() == MAX_CANDIDATES
            && score == top[MAX_CANDIDATES - 1].0
            && r > top[MAX_CANDIDATES - 1].1
        {
            continue;
        }
        push_top_k(
            &mut top,
            MAX_CANDIDATES,
            (
                score,
                r,
                SortKey::from_lowered(&member.display_name, name_lowered),
                MentionEntry::Member(member),
            ),
            cmp,
        );
    }

    if ctx.can_mention_room
        && let Some((score, _)) = score_field(
            &pattern,
            &mut matcher,
            &mut buf,
            &mut lower_buf,
            "room",
            &lowered,
        )
    {
        push_top_k(
            &mut top,
            MAX_CANDIDATES,
            (
                score,
                usize::MAX,
                SortKey::Ascii("@room"),
                MentionEntry::Room,
            ),
            cmp,
        );
    }

    top.into_iter()
        .map(|(_, _, _, entry)| entry.into_candidate())
        .collect()
}

fn member_candidate(member: &Member) -> Candidate {
    Candidate {
        primary: member.display_name.clone(),
        secondary: member.user_id.clone(),
        target: Target::User {
            user_id: member.user_id.clone(),
            display_name: member.display_name.clone(),
        },
    }
}

fn room_mention_candidate() -> Candidate {
    Candidate {
        primary: "@room".to_owned(),
        secondary: "Notify everyone in this room".to_owned(),
        target: Target::RoomMention,
    }
}

fn match_rooms(query: &str, rooms: &[RoomTarget]) -> Vec<Candidate> {
    if query.is_empty() {
        let mut out: Vec<Candidate> = rooms
            .iter()
            .take(MAX_CANDIDATES)
            .map(room_candidate)
            .collect();
        out.sort_by_key(|a| a.primary.to_lowercase());
        return out;
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();
    let mut lower_buf = String::new();
    let lowered = query.to_lowercase();
    let mut top: Vec<(u32, SortKey<'_>, &RoomTarget)> = Vec::with_capacity(MAX_CANDIDATES);

    for room in rooms {
        let alias_hit = room.alias.as_deref().and_then(|a| {
            score_field(
                &pattern,
                &mut matcher,
                &mut buf,
                &mut lower_buf,
                a,
                &lowered,
            )
        });
        let name_hit = score_field(
            &pattern,
            &mut matcher,
            &mut buf,
            &mut lower_buf,
            &room.name,
            &lowered,
        );
        let (alias_score, alias_lowered) = match alias_hit {
            Some((score, lowered_alias)) => (Some(score), lowered_alias),
            None => (None, None),
        };
        let (name_score, name_lowered) = match name_hit {
            Some((score, lowered_name)) => (Some(score), lowered_name),
            None => (None, None),
        };
        let Some(score) = alias_score.into_iter().chain(name_score).max() else {
            continue;
        };
        if top.len() == MAX_CANDIDATES && score < top[MAX_CANDIDATES - 1].0 {
            continue;
        }
        let sort_key = match room.alias.as_deref() {
            Some(alias) => SortKey::from_lowered(alias, alias_lowered),
            None => SortKey::from_lowered(&room.name, name_lowered),
        };
        push_top_k(&mut top, MAX_CANDIDATES, (score, sort_key, room), |a, b| {
            b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))
        });
    }

    top.into_iter()
        .map(|(_, _, room)| room_candidate(room))
        .collect()
}

fn room_candidate(room: &RoomTarget) -> Candidate {
    let label = room.alias.clone().unwrap_or_else(|| room.name.clone());
    let secondary = match &room.alias {
        Some(alias) if alias != &room.name => room.name.clone(),
        _ => String::new(),
    };
    let uri = room_matrix_to_uri(room.alias.as_deref(), &room.id);
    Candidate {
        primary: label.clone(),
        secondary,
        target: Target::RoomLink { label, uri },
    }
}

/// Builds a `https://matrix.to/#/...` URI for a room, preferring its canonical alias.
pub fn room_matrix_to_uri(alias: Option<&str>, room_id: &str) -> String {
    if let Some(alias) = alias
        && let Ok(parsed) = RoomAliasId::parse(alias)
    {
        return parsed.matrix_to_uri().to_string();
    }
    if let Ok(parsed) = RoomId::parse(room_id) {
        return parsed.matrix_to_uri().to_string();
    }
    let clean = sanitize_line(alias.unwrap_or(room_id)).replace(' ', "%20");
    if let Some(rest) = clean.strip_prefix('#') {
        format!("https://matrix.to/#/%23{rest}")
    } else {
        format!("https://matrix.to/#/{clean}")
    }
}

fn score_field(
    pattern: &Pattern,
    matcher: &mut Matcher,
    buf: &mut Vec<char>,
    lower_buf: &mut String,
    field: &str,
    lowered_query: &str,
) -> Option<(u32, Option<String>)> {
    let fuzzy = pattern.score(Utf32Str::new(field, buf), matcher)?;
    let mut owned = None;
    let lowered = if field.is_ascii() {
        lower_buf.clear();
        lower_buf.push_str(field);
        lower_buf.make_ascii_lowercase();
        lower_buf.as_str()
    } else {
        owned.insert(field.to_lowercase()).as_str()
    };
    let boost = if lowered == lowered_query {
        30_000
    } else if lowered.starts_with(lowered_query) {
        20_000
    } else if lowered.contains(lowered_query) {
        10_000
    } else {
        0
    };
    Some((boost + fuzzy, owned))
}

fn push_top_k<T, F>(top: &mut Vec<T>, k: usize, item: T, cmp: F)
where
    F: Fn(&T, &T) -> std::cmp::Ordering,
{
    if top.len() == k && cmp(&top[k - 1], &item) == std::cmp::Ordering::Less {
        return;
    }
    let pos = top
        .binary_search_by(|existing| cmp(existing, &item))
        .unwrap_or_else(|e| e);
    if pos < k {
        top.insert(pos, item);
        if top.len() > k {
            top.pop();
        }
    }
}

/// Computes a per-byte mask marking positions inside fenced code blocks or inline code spans.
/// When `treat_unclosed_as_code` is true, an unclosed fence or inline backtick span suppresses
/// triggers through the cursor while the user is still typing code.
pub fn code_mask(text: &str, treat_unclosed_as_code: bool) -> Vec<bool> {
    let mut mask = vec![false; text.len()];
    let mut in_fence: Option<(u8, usize)> = None;
    let mut offset = 0usize;

    for raw_line in text.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);

        if let Some((fence_ch, fence_len)) = in_fence {
            mask[offset..offset + raw_line.len()].fill(true);
            if is_closing_fence(line, fence_ch, fence_len) {
                in_fence = None;
            }
            offset += raw_line.len();
            continue;
        }

        if let Some((fence_ch, fence_len)) = opening_fence(line) {
            mask[offset..offset + raw_line.len()].fill(true);
            in_fence = Some((fence_ch, fence_len));
            offset += raw_line.len();
            continue;
        }

        mark_inline_code(
            raw_line,
            &mut mask[offset..offset + raw_line.len()],
            treat_unclosed_as_code,
        );
        offset += raw_line.len();
    }
    mask
}

fn opening_fence(line: &str) -> Option<(u8, usize)> {
    let trimmed = strip_up_to_3_spaces(line);
    let first = *trimmed.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let len = trimmed.bytes().take_while(|&b| b == first).count();
    if len < 3 {
        return None;
    }
    let info = &trimmed[len..];
    if first == b'`' && info.contains('`') {
        return None;
    }
    Some((first, len))
}

fn is_closing_fence(line: &str, fence_ch: u8, min_len: usize) -> bool {
    let trimmed = strip_up_to_3_spaces(line);
    let len = trimmed.bytes().take_while(|&b| b == fence_ch).count();
    len >= min_len && trimmed[len..].trim().is_empty()
}

fn strip_up_to_3_spaces(s: &str) -> &str {
    let spaces = s.bytes().take(3).take_while(|&b| b == b' ').count();
    &s[spaces..]
}

fn mark_inline_code(line: &str, mask: &mut [bool], treat_unclosed_as_code: bool) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] != b'`' {
            i += 1;
            continue;
        }
        let open_start = i;
        let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
        i += run;

        // Look ahead for a matching closing backtick run of identical length.
        let mut close = None;
        let mut j = i;
        while j < bytes.len() {
            if bytes[j] == b'`' {
                let close_run = bytes[j..].iter().take_while(|&&b| b == b'`').count();
                if close_run == run {
                    close = Some(j + close_run);
                    break;
                }
                j += close_run;
            } else {
                j += 1;
            }
        }
        if let Some(end) = close {
            mask[open_start..end].fill(true);
            i = end;
        } else if treat_unclosed_as_code {
            mask[open_start..].fill(true);
            break;
        }
    }
}

/// Applies `marks` to `text` outside code spans/blocks, returning `(markdown_with_links, mentions)`.
/// Each accepted mark produces at most one mention: first at its recorded byte offset (`mark.at`),
/// or failing that at the first unused boundary-valid occurrence left to right.
/// Plain-text body stays `text`; Markdown link replacement is intended solely for HTML rendering.
pub fn resolve_marks(text: &str, marks: &[MentionMark]) -> (String, Mentions) {
    if marks.is_empty() || text.is_empty() {
        return (text.to_owned(), Mentions::new());
    }
    let matched = match_marks(text, marks);

    let mut user_ids: BTreeSet<OwnedUserId> = BTreeSet::new();
    let mut room_mention = false;
    let mut replacements: Vec<(Range<usize>, String)> = Vec::new();

    for (idx, (mark, pos)) in marks.iter().zip(matched).enumerate() {
        let Some(pos) = pos else {
            continue;
        };
        let end = pos + mark.needle().len();
        match &mark.kind {
            MentionKind::User { label, user_id } => {
                if let Ok(uid) = UserId::parse(user_id) {
                    let uri = uid.matrix_to_uri().to_string();
                    user_ids.insert(uid);
                    replacements.push((pos..end, markdown_link(label, &uri)));
                }
            }
            MentionKind::RoomNotification => {
                room_mention = true;
            }
            MentionKind::RoomLink { label, uri } => {
                if is_safe_matrix_to(uri) {
                    replacements.push((pos..end, markdown_link(label, uri)));
                }
            }
            MentionKind::Emoticon { .. } => replacements.push((pos..end, emote_placeholder(idx))),
        }
    }

    replacements.sort_by_key(|(range, _)| range.start);
    let mut linked = String::with_capacity(text.len() + replacements.len() * 32);
    let mut last = 0;
    for (range, rep) in replacements {
        linked.push_str(&text[last..range.start]);
        linked.push_str(&rep);
        last = range.end;
    }
    linked.push_str(&text[last..]);

    let mut mentions = Mentions::with_user_ids(user_ids);
    mentions.room = room_mention;
    (linked, mentions)
}

/// Anchors `marks` to their matched byte positions inside `text`, shifted by `shift`, and
/// drops any mark that does not match in `text`. Used when prepending or appending
/// another segment so unanchored (`at: None`) or drifted marks cannot bind to words in
/// the other segment.
pub(crate) fn anchor_marks_in_segment(text: &str, marks: &mut Vec<MentionMark>, shift: usize) {
    if marks.is_empty() {
        return;
    }
    if text.is_empty() {
        marks.clear();
        return;
    }
    let matched = match_marks(text, marks);
    let mut anchored = Vec::with_capacity(marks.len());
    for (mut mark, pos) in std::mem::take(marks).into_iter().zip(matched) {
        if let Some(pos) = pos {
            mark.at = Some(pos + shift);
            anchored.push(mark);
        }
    }
    *marks = anchored;
}

/// Matches each mark against `text` using exact accepted positions first, then left-to-right
/// longest-needle matches outside code spans, returning the matched byte offset per mark.
fn match_marks(text: &str, marks: &[MentionMark]) -> Vec<Option<usize>> {
    let mut occupied = code_mask(text, false);

    // Order marks longest-label first while keeping original index to disambiguate identical names.
    let mut ordered: Vec<(usize, &MentionMark, &str)> = marks
        .iter()
        .enumerate()
        .map(|(idx, mark)| (idx, mark, mark.needle()))
        .filter(|(_, _, needle)| !needle.is_empty())
        .collect();
    ordered.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then(a.0.cmp(&b.0)));

    let mut matched = vec![None; marks.len()];

    // Pass 1: honour exact accepted positions first so an earlier ordinary word (e.g. "May") does
    // not steal a completion accepted later in the message.
    for &(idx, mark, needle) in &ordered {
        if matched[idx].is_some() {
            continue;
        }
        let Some(pos) = mark.at else {
            continue;
        };
        let Some(end) = pos.checked_add(needle.len()) else {
            continue;
        };
        if end > text.len()
            || !text.is_char_boundary(pos)
            || !text.is_char_boundary(end)
            || !text[pos..].starts_with(needle)
            || occupied[pos..end].iter().any(|&b| b)
            || !mark_fits(text, pos, end, mark)
        {
            continue;
        }
        matched[idx] = Some(pos);
        occupied[pos..end].fill(true);
    }

    // Pass 2: match any remaining unused mark against the first unused occurrence left to right.
    // Already-used marks are never reused on subsequent occurrences.
    let mut pos = 0;
    while pos < text.len() {
        if occupied[pos] {
            pos += next_char_len(text, pos);
            continue;
        }
        let mut chosen: Option<(usize, &str)> = None;
        for &(idx, mark, needle) in &ordered {
            if matched[idx].is_some() {
                continue;
            }
            if !text[pos..].starts_with(needle) {
                continue;
            }
            let end = pos + needle.len();
            if occupied[pos..end].iter().any(|&b| b) {
                continue;
            }
            if !mark_fits(text, pos, end, mark) {
                continue;
            }
            chosen = Some((idx, needle));
            break;
        }
        if let Some((idx, needle)) = chosen {
            let end = pos + needle.len();
            matched[idx] = Some(pos);
            occupied[pos..end].fill(true);
            pos = end;
        } else {
            pos += next_char_len(text, pos);
        }
    }

    matched
}

/// Custom emoji are HTML that Markdown would escape, so [`resolve_marks`] leaves a
/// placeholder for each, naming the mark by its index, and `crate::format` puts the
/// picture in after rendering. Private-use characters pass Markdown untouched.
pub const EMOTE_OPEN: char = '\u{E000}';
pub const EMOTE_CLOSE: char = '\u{E001}';

fn emote_placeholder(index: usize) -> String {
    format!("{EMOTE_OPEN}{index}{EMOTE_CLOSE}")
}

fn next_char_len(text: &str, pos: usize) -> usize {
    text[pos..].chars().next().map_or(1, char::len_utf8)
}

/// Whether `mark` may stand at `text[start..end]`. A custom emoji also needs to start a
/// word as its completion did (or follow another one): after a `/` it would be part of
/// an address.
fn mark_fits(text: &str, start: usize, end: usize, mark: &MentionMark) -> bool {
    let before = text[..start].chars().next_back();
    is_mark_boundary(text, start, end)
        && (!matches!(mark.kind, MentionKind::Emoticon { .. })
            || is_trigger_boundary(before)
            || matches!(before, Some(':' | '*' | '~' | '|' | '>')))
}

fn is_mark_boundary(text: &str, start: usize, end: usize) -> bool {
    let prev = text[..start].chars().next_back();
    let next = text[end..].chars().next();
    let left_ok = match prev {
        None => true,
        Some('[') => false,
        Some(ch) => !ch.is_alphanumeric() && ch != '_',
    };
    let right_ok = match next {
        None => true,
        Some(']') if text[end..].starts_with("](") => false,
        Some(ch) => !ch.is_alphanumeric() && ch != '_',
    };
    left_ok && right_ok
}

fn is_safe_matrix_to(uri: &str) -> bool {
    uri.starts_with("https://matrix.to/#/")
        && !uri.contains(|c: char| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'))
}

fn markdown_link(label: &str, uri: &str) -> String {
    let escaped = escape_link_label(label);
    format!("[{escaped}](<{uri}>)")
}

fn escape_link_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for ch in label.chars() {
        // (A bar too: a name with `||` in it must not start a spoiler.)
        if matches!(
            ch,
            '\\' | '[' | ']' | '*' | '_' | '`' | '~' | '<' | '>' | '|'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Extracts `MentionMark`s from an existing message (used to preserve and update mentions
/// when beginning an edit).
pub fn marks_from_message(html: Option<&str>, mentions: Option<&Mentions>) -> Vec<MentionMark> {
    let mut marks = Vec::new();

    if let Some(html) = html {
        for (href, label) in extract_matrix_to_links(html) {
            if label.is_empty() {
                continue;
            }
            if let Some(uid) = parse_matrix_to_user(&href) {
                marks.push(MentionMark::user(label, uid));
            } else {
                marks.push(MentionMark::room_link(label, href));
            }
        }
        for (shortcode, url) in extract_emoticons(html) {
            marks.push(MentionMark::emoticon(&shortcode, url));
        }
    }

    if let Some(mentions) = mentions {
        if mentions.room {
            marks.push(MentionMark::room_notification());
        }
        for uid in &mentions.user_ids {
            let uid_str = uid.to_string();
            let already = marks.iter().any(
                |m| matches!(&m.kind, MentionKind::User { user_id, .. } if user_id == &uid_str),
            );
            if !already {
                marks.push(MentionMark::user(uid_str.clone(), uid_str));
            }
        }
    }

    marks
}

fn extract_matrix_to_links(html: &str) -> Vec<(String, String)> {
    let stripped = strip_mx_reply(html);
    let mut out = Vec::new();
    let mut rest = stripped;

    while let Some(open) = rest.find('<') {
        let tag_tail = &rest[open + 1..];
        let Some(tag_end) = tag_tail.find('>') else {
            break;
        };
        let tag = tag_tail[..tag_end].trim();
        let after_open = &tag_tail[tag_end + 1..];

        if is_tag(tag, "a")
            && let Some(close_rel) = find_close_tag(after_open, "a")
            && let Some(href) = extract_href(tag)
            && is_safe_matrix_to(&href)
        {
            let inner = strip_tags(&after_open[..close_rel]);
            let label = decode_entities(inner.trim());
            out.push((href, label));
            rest = &after_open[close_rel..];
            continue;
        }
        rest = after_open;
    }

    out
}

/// The custom emoji of a message's HTML, as `(shortcode, url)`, so an edit keeps them.
fn extract_emoticons(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = strip_mx_reply(html);
    while let Some(open) = rest.find('<') {
        let tail = &rest[open + 1..];
        let Some(end) = tail.find('>') else {
            break;
        };
        let tag = tail[..end].trim().trim_end_matches('/');
        rest = &tail[end + 1..];
        if !is_tag(tag, "img") {
            continue;
        }
        let attributes = &tag[3..];
        if !crate::rich::is_emoticon(attributes) {
            continue;
        }
        let url = crate::rich::attribute(attributes, "src").map(crate::rich::decode_entities);
        let code = ["alt", "title"].into_iter().find_map(|name| {
            let raw = crate::rich::decode_entities(crate::rich::attribute(attributes, name)?);
            let code = raw.trim().trim_matches(':');
            let ok = !code.is_empty()
                && code.len() <= crate::matrix::MAX_SHORTCODE_LEN
                && !code.contains(|c: char| c.is_whitespace() || c == ':' || c.is_control());
            ok.then(|| code.to_owned())
        });
        if let (Some(url), Some(code)) = (url, code)
            && crate::matrix::is_valid_mxc(&url)
        {
            out.push((code, url));
        }
    }
    out
}

fn strip_mx_reply(html: &str) -> &str {
    let lower = html.to_ascii_lowercase();
    if let Some(start) = lower.find("<mx-reply")
        && let Some(end) = lower[start..].find("</mx-reply>")
        && start == 0
    {
        return &html[start + end + "</mx-reply>".len()..];
    }
    html
}

fn is_tag(tag: &str, name: &str) -> bool {
    // `get`: a tag of the sender's may start with a character wider than one byte.
    tag.get(..name.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(name))
        && tag[name.len()..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
}

fn find_close_tag(s: &str, name: &str) -> Option<usize> {
    let needle = format!("</{name}>");
    let lower = s.to_ascii_lowercase();
    lower.find(&needle)
}

fn extract_href(tag: &str) -> Option<String> {
    let (_, attributes) = tag.split_once(char::is_whitespace)?;
    let href = crate::rich::attribute(attributes, "href")?.trim();
    (!href.is_empty()).then(|| crate::rich::decode_entities(href))
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    for ch in s.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
}

fn parse_matrix_to_user(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("https://matrix.to/#/")?;
    let target = rest.split(['?', '/']).next()?;
    let decoded = percent_decode(target);
    UserId::parse(&decoded).ok().map(|u| u.to_string())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    static NO_EMOTES: EmoteCatalog = EmoteCatalog { packs: Vec::new() };

    fn test_ctx<'a>(
        members: &'a [Member],
        recent: &'a [String],
        rooms: &'a [RoomTarget],
    ) -> Context<'a> {
        Context {
            members,
            recent_speakers: recent,
            can_mention_room: true,
            rooms,
            emotes: &NO_EMOTES,
        }
    }

    fn member(uid: &str, name: &str) -> Member {
        Member {
            user_id: uid.to_owned(),
            display_name: name.to_owned(),
        }
    }

    fn room(id: &str, name: &str, alias: Option<&str>) -> RoomTarget {
        RoomTarget {
            id: id.to_owned(),
            name: name.to_owned(),
            alias: alias.map(str::to_owned),
        }
    }

    #[test]
    fn trigger_detection_respects_word_starts_and_code_spans() {
        let members = vec![member("@alice:hs", "Alice")];
        let rooms = vec![room("!r:hs", "Rust", Some("#rust:hs"))];
        let ctx = test_ctx(&members, &[], &rooms);

        // Word starts succeed.
        assert_eq!(find("@", 1, &ctx).unwrap().kind, Kind::Mention);
        assert_eq!(find("hi @al", 6, &ctx).unwrap().kind, Kind::Mention);
        assert_eq!(find("(@al", 4, &ctx).unwrap().kind, Kind::Mention);
        assert_eq!(find("hi #ru", 6, &ctx).unwrap().kind, Kind::Room);

        // Not a word start (email address, mid-word hash).
        assert!(find("foo@al", 6, &ctx).is_none());
        assert!(find("foo#ru", 6, &ctx).is_none());
        assert!(find("https://example.org/#ru", 23, &ctx).is_none());

        // Code spans (closed and currently open) and fenced blocks suppress completion.
        assert!(find("`@al", 4, &ctx).is_none());
        assert!(find("`hi @al`", 7, &ctx).is_none());
        // Outside closed inline code span works again.
        assert_eq!(find("`code` @al", 10, &ctx).unwrap().kind, Kind::Mention);
        // Inside fenced block is suppressed; after closing fence works again.
        assert!(find("```\n@al", 7, &ctx).is_none());
        assert_eq!(
            find("```\ncode\n```\n@al", 16, &ctx).unwrap().kind,
            Kind::Mention
        );
    }

    #[test]
    fn emoji_trigger_requires_two_chars_and_skips_urls() {
        let ctx = test_ctx(&[], &[], &[]);
        assert!(find(":", 1, &ctx).is_none());
        assert!(find(":s", 2, &ctx).is_none());
        let found = find(":smi", 4, &ctx).expect("two chars trigger emoji");
        assert_eq!(found.kind, Kind::Emoji);
        assert!(
            found
                .items
                .iter()
                .any(|c| c.primary == "😄" || c.primary == "😊")
        );

        // Never inside URLs, times, or already-closed shortcodes.
        assert!(find("https://foo", 11, &ctx).is_none());
        assert!(find("12:30", 5, &ctx).is_none());
        assert!(find(":smile:ok", 9, &ctx).is_none());
    }

    #[test]
    fn slash_commands_trigger_only_at_very_start_until_space() {
        let ctx = test_ctx(&[], &[], &[]);
        let found = find("/", 1, &ctx).unwrap();
        assert_eq!(found.kind, Kind::Command);
        assert_eq!(found.items.len(), COMMANDS.len());

        let up = find("/up", 3, &ctx).unwrap();
        assert_eq!(up.items[0].primary, "/upload");

        // After space, slash popup closes so /upload path completion takes over.
        assert!(find("/upload foo", 11, &ctx).is_none());
        // Mid-message or double slash does not trigger command completion.
        assert!(find("hi /me", 6, &ctx).is_none());
        assert!(find("//me", 4, &ctx).is_none());
    }

    #[test]
    fn wide_chars_before_trigger_preserve_byte_ranges() {
        let members = vec![member("@bob:hs", "Bob")];
        let ctx = test_ctx(&members, &[], &[]);
        let text = "Hello 🎉 @bo";
        let found = find(text, text.len(), &ctx).unwrap();
        assert_eq!(&text[found.range.clone()], "@bo");
        let (ins, mark) = replacement(text, &found.range, &found.items[0].target);
        assert_eq!(ins, "Bob ");
        assert!(matches!(
            mark.map(|m| m.kind),
            Some(MentionKind::User { .. })
        ));

        // Cursor in the middle of text, before trailing wide emoji.
        let both = "Hey @bo 👋!";
        let cursor = "Hey @bo".len();
        let found_mid = find(both, cursor, &ctx).unwrap();
        assert_eq!(found_mid.range, "Hey ".len()..cursor);
    }

    #[test]
    fn mention_ranking_prefers_recent_speakers_and_caps_huge_rooms() {
        let mut members: Vec<Member> = (0..12_000)
            .map(|i| member(&format!("@user{i:05}:hs"), &format!("User {i:05}")))
            .collect();
        members.push(member("@zoe:hs", "Zoe"));
        members.push(member("@amy:hs", "Amy"));
        let recent = vec!["Zoe".to_owned(), "User 00999".to_owned()];
        let ctx = Context {
            members: &members,
            recent_speakers: &recent,
            can_mention_room: true,
            rooms: &[],
            emotes: &NO_EMOTES,
        };

        let empty = find("@", 1, &ctx).unwrap();
        assert_eq!(empty.items.len(), MAX_CANDIDATES);
        assert_eq!(empty.items[0].primary, "Zoe");
        assert_eq!(empty.items[1].primary, "User 00999");

        // Match by user id when display name differs.
        let by_id = find("@zoe", 4, &ctx).unwrap();
        assert_eq!(by_id.items[0].primary, "Zoe");

        // @room only offered when permitted.
        let mut restricted = Context {
            members: &members,
            recent_speakers: &recent,
            can_mention_room: false,
            rooms: &[],
            emotes: &NO_EMOTES,
        };
        let no_room = find("@room", 5, &restricted);
        assert!(no_room.is_none());
        restricted.can_mention_room = true;
        let yes_room = find("@room", 5, &restricted).unwrap();
        assert_eq!(yes_room.items[0].primary, "@room");
    }

    #[test]
    fn resolve_marks_builds_links_and_skips_code() {
        let marks = vec![
            MentionMark::user("Alice [bot]", "@alice:hs"),
            MentionMark::room_link("#rust:hs", "https://matrix.to/#/%23rust:hs"),
            MentionMark::room_notification(),
        ];
        let text = "Hey Alice [bot], check #rust:hs and @room! Also `Alice [bot]`";
        let (linked, mentions) = resolve_marks(text, &marks);
        assert!(linked.contains("[Alice \\[bot\\]](<https://matrix.to/#/@alice:hs>)"));
        assert!(linked.contains("[#rust:hs](<https://matrix.to/#/%23rust:hs>)"));
        assert!(linked.ends_with("`Alice [bot]`"));
        assert!(mentions.room);
        assert_eq!(mentions.user_ids.len(), 1);
        assert!(mentions.user_ids.iter().any(|u| u.as_str() == "@alice:hs"));
    }

    #[test]
    fn resolve_marks_common_word_mentions_at_most_once_per_accepted_completion() {
        // Common word ("Rat" / "May") accepted once must NOT turn later plain occurrences into
        // mentions; a second mention requires a second accepted completion.
        let one_at_start = vec![MentionMark::user("Rat", "@rat:hs").with_at(Some(3))];
        let (linked, mentions) = resolve_marks("Hi Rat, a rat is a Rat", &one_at_start);
        assert_eq!(
            linked,
            "Hi [Rat](<https://matrix.to/#/@rat:hs>), a rat is a Rat"
        );
        assert_eq!(mentions.user_ids.len(), 1);

        // Plain word first, accepted completion second: honours `mark.at` on the second one.
        let second_only = vec![MentionMark::user("May", "@may:hs").with_at(Some(10))];
        let (linked_second, _) = resolve_marks("May I ask May?", &second_only);
        assert_eq!(
            linked_second,
            "May I ask [May](<https://matrix.to/#/@may:hs>)?"
        );

        // Failing the anchored offset falls back to the first unused occurrence, still once.
        let stale_offset = vec![MentionMark::user("Will", "@will:hs").with_at(Some(99))];
        let (linked_fallback, _) = resolve_marks("Will asked Will to come", &stale_offset);
        assert_eq!(
            linked_fallback,
            "[Will](<https://matrix.to/#/@will:hs>) asked Will to come"
        );

        // Two accepted completions for the same person mention both accepted slots.
        let twice = vec![
            MentionMark::user("Rat", "@rat:hs").with_at(Some(0)),
            MentionMark::user("Rat", "@rat:hs").with_at(Some(9)),
        ];
        let (linked_twice, mentions_twice) = resolve_marks("Rat said Rat again", &twice);
        assert_eq!(
            linked_twice,
            "[Rat](<https://matrix.to/#/@rat:hs>) said [Rat](<https://matrix.to/#/@rat:hs>) again"
        );
        assert_eq!(mentions_twice.user_ids.len(), 1);
    }

    #[test]
    fn marks_from_message_extracts_for_edit_and_ignores_mx_reply() {
        let html = "<mx-reply><blockquote><a href=\"https://matrix.to/#/@ignored:hs\">Ignored</a></blockquote></mx-reply>Hi <a href=\"https://matrix.to/#/@alice:hs\">Alice</a> see <a href=\"https://matrix.to/#/%23rust:hs\">#rust:hs</a>";
        let orig = Mentions::with_user_ids(["@alice:hs".try_into().unwrap()]);
        let marks = marks_from_message(Some(html), Some(&orig));
        assert_eq!(
            marks,
            vec![
                MentionMark::user("Alice", "@alice:hs"),
                MentionMark::room_link("#rust:hs", "https://matrix.to/#/%23rust:hs"),
            ]
        );
    }

    fn catalog(codes: &[&str]) -> EmoteCatalog {
        use crate::matrix::{CustomEmoji, EmotePack};
        EmoteCatalog {
            packs: vec![EmotePack {
                id: "user".into(),
                name: "Mine".into(),
                emojis: codes
                    .iter()
                    .map(|code| CustomEmoji {
                        shortcode: (*code).into(),
                        url: format!("mxc://example.org/{code}"),
                        body: None,
                        pack_name: "Mine".into(),
                    })
                    .collect(),
                stickers: Vec::new(),
            }],
        }
    }

    #[test]
    fn custom_emoji_starting_with_the_query_come_before_the_standard_ones() {
        let emotes = catalog(&["smile_rat", "blobsmile", "partyparrot"]);
        let ctx = Context {
            members: &[],
            recent_speakers: &[],
            can_mention_room: false,
            rooms: &[],
            emotes: &emotes,
        };
        let found = find("hi :smi", 7, &ctx).expect("emoji completion");
        assert_eq!(found.kind, Kind::Emoji);
        assert!(found.items.len() <= MAX_CANDIDATES);
        let primaries: Vec<&str> = found.items.iter().map(|i| i.primary.as_str()).collect();
        assert_eq!(primaries[..2], [":smile_rat:", ":blobsmile:"]);
        assert_eq!(found.items[0].secondary, "custom · Mine");
        assert!(matches!(found.items[2].target, Target::Emoji(_)));

        // Something only a custom emoji matches still opens the list.
        let emotes = catalog(&["qxzparrot"]);
        let ctx = Context {
            emotes: &emotes,
            ..ctx
        };
        let found = find(":qxzp", 5, &ctx).expect("custom only");
        assert_eq!(found.items.len(), 1);
        let (insert, mark) = replacement(":qxzp", &found.range, &found.items[0].target);
        assert_eq!(insert, ":qxzparrot: ");
        assert_eq!(
            mark,
            Some(
                MentionMark::emoticon("qxzparrot", "mxc://example.org/qxzparrot").with_at(Some(0))
            )
        );
        assert_eq!(trigger(":pa", 3), Some(Kind::Emoji));
        assert_eq!(trigger(":p", 2), None);
        assert_eq!(trigger("`:pa", 4), None);
    }

    #[test]
    fn an_edit_keeps_the_custom_emoji_of_the_message() {
        let html = "<mx-reply><img data-mx-emoticon src=\"mxc://example.org/old\" alt=\":old:\"></mx-reply>\
                    hi <img data-mx-emoticon src=\"mxc://example.org/blob\" alt=\":blob:\" title=\":blob:\"> \
                    <img data-mx-emoticon src=\"https://evil.example/x\" alt=\":bad:\"> \
                    <img data-mx-emoticon src=\"mxc://example.org/two\" alt=\"two words\"> \
                    <img src=\"mxc://example.org/pic\" alt=\"a picture\"> \
                    <img src=\"mxc://example.org/named\" alt=\":named:\"> <éé> <é";
        // The last one has lost its mark, as received messages do, and is known by its name.
        assert_eq!(
            marks_from_message(Some(html), None),
            vec![
                MentionMark::emoticon("blob", "mxc://example.org/blob"),
                MentionMark::emoticon("named", "mxc://example.org/named")
            ]
        );
        let (linked, _) = resolve_marks("hi :blob:", &marks_from_message(Some(html), None));
        assert_eq!(linked, format!("hi {EMOTE_OPEN}0{EMOTE_CLOSE}"));
    }

    #[test]
    fn extract_href_uses_attribute_parser_not_substring_match() {
        let html = concat!(
            r#"<a data-href="https://matrix.to/#/@wrong:example.org" href="https://matrix.to/#/@alice:example.org">Alice</a> "#,
            r#"<a title="href='https://matrix.to/#/@wrong:example.org'" href="https://matrix.to/#/@bob:example.org">Bob</a>"#,
        );
        let links = extract_matrix_to_links(html);
        assert_eq!(
            links,
            vec![
                (
                    "https://matrix.to/#/@alice:example.org".to_owned(),
                    "Alice".to_owned(),
                ),
                (
                    "https://matrix.to/#/@bob:example.org".to_owned(),
                    "Bob".to_owned(),
                ),
            ]
        );
    }

    #[test]
    fn mention_completion_ranks_case_insensitively_with_unicode_and_room_order() {
        let members = vec![
            member("@b:hs", "beta"),
            member("@a:hs", "Alpha"),
            member("@c:hs", "Ärger"),
            member("@d:hs", "ärger2"),
            member("@e:hs", "charlie"),
        ];
        let ctx = Context {
            members: &members,
            recent_speakers: &["charlie".to_owned()],
            can_mention_room: true,
            rooms: &[],
            emotes: &NO_EMOTES,
        };
        // Empty query: recent speaker first, then case-insensitive alphabetical, then @room last.
        let empty = find("@", 1, &ctx).unwrap();
        let names: Vec<&str> = empty.items.iter().map(|i| i.primary.as_str()).collect();
        assert_eq!(
            names,
            ["charlie", "Alpha", "beta", "Ärger", "ärger2", "@room"]
        );

        // Non-empty query matches Unicode case-insensitively and preserves tie-break order.
        let query = "@ärg";
        let umlaut = find(query, query.len(), &ctx).unwrap();
        let umlaut_names: Vec<&str> = umlaut.items.iter().map(|i| i.primary.as_str()).collect();
        assert_eq!(umlaut_names, ["Ärger", "ärger2"]);
    }

    #[test]
    fn mention_completion_matches_greek_final_sigma_like_str_to_lowercase() {
        let members = vec![
            member("@prefix:hs", "ος1"),
            member("@exact:hs", "ΟΣ"),
            member("@medial:hs", "οσ"),
        ];
        let ctx = Context {
            members: &members,
            recent_speakers: &[],
            can_mention_room: false,
            rooms: &[],
            emotes: &NO_EMOTES,
        };
        // `str::to_lowercase("ΟΣ")` is `"ος"` (final sigma), so `"ΟΣ"` gets the exact
        // boost and ranks ahead of `"ος1"` (prefix boost only).
        let query = "@ος";
        let result = find(query, query.len(), &ctx).unwrap();
        assert_eq!(result.items[0].primary, "ΟΣ");

        // Empty query orders by `str::to_lowercase`: `"ος"` (U+03C2) comes before
        // `"οσ"` (U+03C3).
        let empty = find("@", 1, &ctx).unwrap();
        let names: Vec<&str> = empty.items.iter().map(|i| i.primary.as_str()).collect();
        assert_eq!(names, ["ΟΣ", "ος1", "οσ"]);
    }

    #[test]
    fn mention_completion_keeps_tie_replacement_rule_past_ten_entries() {
        let members: Vec<Member> = (0..12)
            .map(|i| member(&format!("@sam{i:02}:hs"), "Sam"))
            .collect();
        let ctx = Context {
            members: &members,
            recent_speakers: &[],
            can_mention_room: false,
            rooms: &[],
            emotes: &NO_EMOTES,
        };
        let empty = find("@", 1, &ctx).unwrap();
        assert_eq!(empty.items.len(), MAX_CANDIDATES);
        let ids: Vec<&str> = empty.items.iter().map(|i| i.secondary.as_str()).collect();
        assert!(
            ids.contains(&"@sam11:hs"),
            "tied entry beyond top 10 should still replace as before: {ids:?}"
        );
    }
}
