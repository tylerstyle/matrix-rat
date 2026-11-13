//! Composer completions (`@` mentions, `:emoji:`, `#` rooms, `/` commands) and
//! resolution of accepted mentions and room links into Matrix HTML + `m.mentions`.

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use matrix_sdk::ruma::events::Mentions;
use matrix_sdk::ruma::{OwnedUserId, RoomAliasId, RoomId, UserId};
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::text::sanitize_line;

/// Maximum number of completion candidates retained and displayed in the popup.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user_id: String,
    pub display_name: String,
}

/// One joined room eligible for `#` completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomTarget {
    pub id: String,
    pub name: String,
    pub alias: Option<String>,
}

/// What happens when a completion item is accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    User {
        user_id: String,
        display_name: String,
    },
    RoomMention,
    Emoji(String),
    RoomLink {
        label: String,
        uri: String,
    },
    Command(String),
}

/// One row shown in the completion popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub primary: String,
    pub secondary: String,
    pub target: Target,
}

/// An accepted mention or room link tracked alongside composer text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionMark {
    User { label: String, user_id: String },
    RoomNotification,
    RoomLink { label: String, uri: String },
}

/// Active completion state while the user is typing in the composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub kind: Kind,
    /// Byte range in composer text covered by the trigger and query (e.g. `@al`).
    pub range: Range<usize>,
    pub items: Vec<Candidate>,
    pub selected: usize,
}

/// Inputs used to compute completion candidates at the current cursor.
pub struct Context<'a> {
    pub members: &'a [Member],
    /// Most-recent-first unique speaker names in the current timeline.
    pub recent_speakers: &'a [String],
    pub can_mention_room: bool,
    pub rooms: &'a [RoomTarget],
}

/// Slash commands recognised by `crate::format` and `crate::upload`.
const COMMANDS: &[(&str, &str)] = &[
    ("/me", "Send an action message (* you …)"),
    ("/plain", "Send without Markdown formatting"),
    ("/html", "Send raw HTML formatting"),
    ("/spoiler", "Hide text behind a spoiler"),
    ("/rainbow", "Colour each character of the message"),
    ("/shrug", "Prepend ¯\\_(ツ)_/¯ to the message"),
    ("/tableflip", "Prepend (╯°□°)╯︵ ┻━┻ to the message"),
    ("/lenny", "Prepend ( ͡° ͜ʖ ͡°) to the message"),
    ("/upload", "Attach and upload a file"),
    ("/file", "Attach and upload a file"),
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
            Some(MentionMark::User {
                label: display_name.clone(),
                user_id: user_id.clone(),
            }),
        ),
        Target::RoomMention => (
            format!("@room{suffix}"),
            Some(MentionMark::RoomNotification),
        ),
        Target::Emoji(ch) => (ch.clone(), None),
        Target::RoomLink { label, uri } => (
            format!("{label}{suffix}"),
            Some(MentionMark::RoomLink {
                label: label.clone(),
                uri: uri.clone(),
            }),
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
        Kind::Emoji => match_emoji(query),
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

fn match_commands(query: &str) -> Vec<Candidate> {
    let items: Vec<Candidate> = if query.is_empty() {
        COMMANDS
            .iter()
            .take(MAX_CANDIDATES)
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
            .take(MAX_CANDIDATES)
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

fn match_emoji(query: &str) -> Vec<Candidate> {
    crate::emoji::search(query)
        .into_iter()
        .take(MAX_CANDIDATES)
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
        .collect()
}

fn match_mentions(query: &str, ctx: &Context<'_>) -> Vec<Candidate> {
    let recency = |name: &str, uid: &str| -> usize {
        ctx.recent_speakers
            .iter()
            .position(|s| s == name || s == uid)
            .unwrap_or(usize::MAX)
    };

    if query.is_empty() {
        let mut ranked: Vec<(usize, String, Candidate)> = Vec::with_capacity(
            ctx.members.len().min(MAX_CANDIDATES * 4) + usize::from(ctx.can_mention_room),
        );
        // For huge rooms, collect recent speakers first without scanning more than needed,
        // still respecting alphabetical fallback among remaining members.
        for member in ctx.members {
            let r = recency(&member.display_name, &member.user_id);
            let sort_key = member.display_name.to_lowercase();
            let candidate = member_candidate(member);
            push_top_k(
                &mut ranked,
                MAX_CANDIDATES,
                (r, sort_key, candidate),
                |a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)),
            );
        }
        if ctx.can_mention_room {
            // Keep `@room` below regular members on an empty query so hitting Enter
            // accidentally after `@` does not ping the whole room.
            let room_item = (
                usize::MAX,
                "\u{10ffff}room".to_owned(),
                room_mention_candidate(),
            );
            push_top_k(&mut ranked, MAX_CANDIDATES, room_item, |a, b| {
                a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1))
            });
        }
        return ranked.into_iter().map(|(_, _, c)| c).collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();
    let lowered = query.to_lowercase();
    let mut top: Vec<(u32, usize, String, Candidate)> = Vec::with_capacity(MAX_CANDIDATES);

    for member in ctx.members {
        let name_score = score_field(
            &pattern,
            &mut matcher,
            &mut buf,
            &member.display_name,
            &lowered,
        );
        let id_score = score_field(&pattern, &mut matcher, &mut buf, &member.user_id, &lowered);
        let Some(score) = name_score.into_iter().chain(id_score).max() else {
            continue;
        };
        let r = recency(&member.display_name, &member.user_id);
        let key = member.display_name.to_lowercase();
        let entry = (score, r, key, member_candidate(member));
        push_top_k(&mut top, MAX_CANDIDATES, entry, |a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
    }

    if ctx.can_mention_room
        && let Some(score) = score_field(&pattern, &mut matcher, &mut buf, "room", &lowered)
    {
        let entry = (
            score,
            usize::MAX,
            "@room".to_owned(),
            room_mention_candidate(),
        );
        push_top_k(&mut top, MAX_CANDIDATES, entry, |a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
    }

    top.into_iter().map(|(_, _, _, c)| c).collect()
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
    let lowered = query.to_lowercase();
    let mut top: Vec<(u32, String, Candidate)> = Vec::with_capacity(MAX_CANDIDATES);

    for room in rooms {
        let alias_score = room
            .alias
            .as_deref()
            .and_then(|a| score_field(&pattern, &mut matcher, &mut buf, a, &lowered));
        let name_score = score_field(&pattern, &mut matcher, &mut buf, &room.name, &lowered);
        let Some(score) = alias_score.into_iter().chain(name_score).max() else {
            continue;
        };
        let key = room.alias.as_deref().unwrap_or(&room.name).to_lowercase();
        push_top_k(
            &mut top,
            MAX_CANDIDATES,
            (score, key, room_candidate(room)),
            |a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)),
        );
    }

    top.into_iter().map(|(_, _, c)| c).collect()
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
    field: &str,
    lowered_query: &str,
) -> Option<u32> {
    let fuzzy = pattern.score(Utf32Str::new(field, buf), matcher)?;
    let lower = field.to_lowercase();
    let boost = if lower == lowered_query {
        30_000
    } else if lower.starts_with(lowered_query) {
        20_000
    } else if lower.contains(lowered_query) {
        10_000
    } else {
        0
    };
    Some(boost + fuzzy)
}

fn push_top_k<T, F>(top: &mut Vec<T>, k: usize, item: T, cmp: F)
where
    F: Fn(&T, &T) -> std::cmp::Ordering,
{
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
/// Plain-text body stays `text`; Markdown link replacement is intended solely for HTML rendering.
pub fn resolve_marks(text: &str, marks: &[MentionMark]) -> (String, Mentions) {
    if marks.is_empty() || text.is_empty() {
        return (text.to_owned(), Mentions::new());
    }
    let mask = code_mask(text, false);

    // Order marks longest-label first while keeping original index to disambiguate identical names.
    let mut ordered: Vec<(usize, &MentionMark, &str)> = marks
        .iter()
        .enumerate()
        .map(|(idx, mark)| {
            let needle = match mark {
                MentionMark::User { label, .. } | MentionMark::RoomLink { label, .. } => {
                    label.as_str()
                }
                MentionMark::RoomNotification => "@room",
            };
            (idx, mark, needle)
        })
        .filter(|(_, _, needle)| !needle.is_empty())
        .collect();
    ordered.sort_by(|a, b| b.2.len().cmp(&a.2.len()).then(a.0.cmp(&b.0)));

    let mut used = vec![false; marks.len()];
    let mut user_ids: BTreeSet<OwnedUserId> = BTreeSet::new();
    let mut room_mention = false;
    let mut replacements: Vec<(Range<usize>, String)> = Vec::new();
    let mut occupied = mask;

    // Collect non-overlapping matches left to right; unused marks match before reused duplicates.
    let mut pos = 0;
    while pos < text.len() {
        if occupied[pos] {
            pos += next_char_len(text, pos);
            continue;
        }
        let mut chosen: Option<(usize, &MentionMark, &str)> = None;
        for pass in 0..2 {
            for &(idx, mark, needle) in &ordered {
                if (pass == 0 && used[idx]) || (pass == 1 && !used[idx]) {
                    continue;
                }
                if !text[pos..].starts_with(needle) {
                    continue;
                }
                let end = pos + needle.len();
                if occupied[pos..end].iter().any(|&b| b) {
                    continue;
                }
                if !is_mark_boundary(text, pos, end) {
                    continue;
                }
                chosen = Some((idx, mark, needle));
                break;
            }
            if chosen.is_some() {
                break;
            }
        }
        if let Some((idx, mark, needle)) = chosen {
            let end = pos + needle.len();
            used[idx] = true;
            occupied[pos..end].fill(true);
            match mark {
                MentionMark::User { label, user_id } => {
                    if let Ok(uid) = UserId::parse(user_id) {
                        let uri = uid.matrix_to_uri().to_string();
                        user_ids.insert(uid);
                        replacements.push((pos..end, markdown_link(label, &uri)));
                    }
                }
                MentionMark::RoomNotification => {
                    room_mention = true;
                }
                MentionMark::RoomLink { label, uri } => {
                    if is_safe_matrix_to(uri) {
                        replacements.push((pos..end, markdown_link(label, uri)));
                    }
                }
            }
            pos = end;
        } else {
            pos += next_char_len(text, pos);
        }
    }

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

fn next_char_len(text: &str, pos: usize) -> usize {
    text[pos..].chars().next().map_or(1, char::len_utf8)
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
        if matches!(ch, '\\' | '[' | ']' | '*' | '_' | '`' | '~' | '<' | '>') {
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
    let mut seen_users: HashSet<String> = HashSet::new();
    let mut seen_rooms: HashSet<String> = HashSet::new();

    if let Some(html) = html {
        for (href, label) in extract_matrix_to_links(html) {
            if label.is_empty() {
                continue;
            }
            if let Some(uid) = parse_matrix_to_user(&href) {
                if seen_users.insert(format!("{uid}\0{label}")) {
                    marks.push(MentionMark::User {
                        label,
                        user_id: uid,
                    });
                }
            } else if seen_rooms.insert(format!("{href}\0{label}")) {
                marks.push(MentionMark::RoomLink { label, uri: href });
            }
        }
    }

    if let Some(mentions) = mentions {
        if mentions.room {
            marks.push(MentionMark::RoomNotification);
        }
        for uid in &mentions.user_ids {
            let uid_str = uid.to_string();
            let already = marks
                .iter()
                .any(|m| matches!(m, MentionMark::User { user_id, .. } if user_id == &uid_str));
            if !already {
                marks.push(MentionMark::User {
                    label: uid_str.clone(),
                    user_id: uid_str,
                });
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
    tag.len() >= name.len()
        && tag[..name.len()].eq_ignore_ascii_case(name)
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
    let lower = tag.to_ascii_lowercase();
    let idx = lower.find("href=")?;
    let val = tag[idx + 5..].trim_start();
    let quote = val.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let inner = val[1..].split(quote).next()?;
    Some(decode_entities(inner.trim()))
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
        assert_eq!(found.items.len(), MAX_CANDIDATES);

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
        assert!(matches!(mark, Some(MentionMark::User { .. })));

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
            MentionMark::User {
                label: "Alice [bot]".to_owned(),
                user_id: "@alice:hs".to_owned(),
            },
            MentionMark::RoomLink {
                label: "#rust:hs".to_owned(),
                uri: "https://matrix.to/#/%23rust:hs".to_owned(),
            },
            MentionMark::RoomNotification,
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
    fn marks_from_message_extracts_for_edit_and_ignores_mx_reply() {
        let html = "<mx-reply><blockquote><a href=\"https://matrix.to/#/@ignored:hs\">Ignored</a></blockquote></mx-reply>Hi <a href=\"https://matrix.to/#/@alice:hs\">Alice</a> see <a href=\"https://matrix.to/#/%23rust:hs\">#rust:hs</a>";
        let orig = Mentions::with_user_ids(["@alice:hs".try_into().unwrap()]);
        let marks = marks_from_message(Some(html), Some(&orig));
        assert_eq!(
            marks,
            vec![
                MentionMark::User {
                    label: "Alice".to_owned(),
                    user_id: "@alice:hs".to_owned()
                },
                MentionMark::RoomLink {
                    label: "#rust:hs".to_owned(),
                    uri: "https://matrix.to/#/%23rust:hs".to_owned()
                },
            ]
        );
    }
}
