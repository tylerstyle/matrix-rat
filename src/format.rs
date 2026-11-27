//! Turning what the user typed into a Matrix message: Markdown, plus slash commands.
//!
//! Matrix carries formatting as HTML next to a plain-text body; bridges translate that
//! HTML into each network's own formatting.

use comrak::Options;
use matrix_sdk::ruma::events::Mentions;

use crate::complete::{EMOTE_CLOSE, EMOTE_OPEN, MentionKind, MentionMark, resolve_marks};

/// What to do with a line of input.
#[derive(Clone)]
pub enum Outgoing {
    /// A normal message. `html` is `None` when the text has no formatting.
    Text {
        plain: String,
        html: Option<String>,
        mentions: Option<Mentions>,
    },
    /// A `/me` action.
    Emote {
        plain: String,
        html: Option<String>,
        mentions: Option<Mentions>,
    },
    /// Nothing is sent; show this to the user (unknown command, missing argument).
    Rejected(String),
}

/// Hand-written: a message is private, and so is whom it mentions.
impl std::fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (name, plain, html, mentions) = match self {
            Self::Text {
                plain,
                html,
                mentions,
            } => ("Text", plain, html, mentions),
            Self::Emote {
                plain,
                html,
                mentions,
            } => ("Emote", plain, html, mentions),
            Self::Rejected(_) => return f.write_str("Rejected"),
        };
        write!(
            f,
            "{name}({} bytes, html: {}, mentions: {})",
            plain.len(),
            html.is_some(),
            mentions.as_ref().map_or(0, |m| m.user_ids.len())
        )
    }
}

impl PartialEq for Outgoing {
    fn eq(&self, other: &Self) -> bool {
        let same_mentions = |a: &Option<Mentions>, b: &Option<Mentions>| {
            a.as_ref().map(|m| (&m.user_ids, m.room)) == b.as_ref().map(|m| (&m.user_ids, m.room))
        };
        match (self, other) {
            (
                Self::Text {
                    plain: p1,
                    html: h1,
                    mentions: m1,
                },
                Self::Text {
                    plain: p2,
                    html: h2,
                    mentions: m2,
                },
            ) => p1 == p2 && h1 == h2 && same_mentions(m1, m2),
            (
                Self::Emote {
                    plain: p1,
                    html: h1,
                    mentions: m1,
                },
                Self::Emote {
                    plain: p2,
                    html: h2,
                    mentions: m2,
                },
            ) => p1 == p2 && h1 == h2 && same_mentions(m1, m2),
            (Self::Rejected(r1), Self::Rejected(r2)) => r1 == r2,
            _ => false,
        }
    }
}

impl Eq for Outgoing {}

const SHRUG: &str = "¯\\_(ツ)_/¯";
/// [`SHRUG`] escaped for Markdown so `\` stays visible and `_` is not read as emphasis.
const SHRUG_MD: &str = r"¯\\\_(ツ)\_/¯";
const TABLEFLIP: &str = "(╯°□°)╯︵ ┻━┻";
const LENNY: &str = "( ͡° ͜ʖ ͡°)";

#[cfg(test)]
pub fn parse(input: &str) -> Outgoing {
    parse_with_mentions(input, &[], false)
}

/// Parses `input` and resolves accepted `marks` into `matrix.to` HTML links and `m.mentions`.
/// When `preserve_empty_mentions` is true (e.g. editing a message that previously carried
/// mentions), empty mentions are emitted as `Some(Mentions::new())` so the edit clears them.
pub fn parse_with_mentions(
    raw_input: &str,
    marks: &[MentionMark],
    preserve_empty_mentions: bool,
) -> Outgoing {
    let input = raw_input.trim();
    // `//text` sends a message that starts with a literal slash.
    if let Some(rest) = input.strip_prefix("//") {
        let shifted = shift_marks(raw_input, rest, 1, marks);
        return markdown_message(&format!("/{rest}"), &shifted, preserve_empty_mentions);
    }
    let Some(command) = input.strip_prefix('/') else {
        let shifted = shift_marks(raw_input, input, 0, marks);
        return markdown_message(input, &shifted, preserve_empty_mentions);
    };
    let (name, argument) = match command.split_once(char::is_whitespace) {
        Some((name, argument)) => (name, argument.trim()),
        None => (command, ""),
    };
    let needs_text = |outgoing: Outgoing| {
        if argument.is_empty() {
            Outgoing::Rejected(format!("/{name} needs some text"))
        } else {
            outgoing
        }
    };
    let arg_marks = shift_marks(raw_input, argument, 0, marks);
    match name {
        "me" => {
            let (linked, mentions) = resolve_marks(argument, &arg_marks);
            needs_text(Outgoing::Emote {
                plain: argument.to_owned(),
                html: message_html(&linked, &arg_marks),
                mentions: finish_mentions(mentions, preserve_empty_mentions),
            })
        }
        "plain" => {
            let (_, mentions) = resolve_marks(argument, &arg_marks);
            needs_text(Outgoing::Text {
                plain: argument.to_owned(),
                html: None,
                mentions: finish_mentions(mentions, preserve_empty_mentions),
            })
        }
        "html" if strip_tags(argument).trim().is_empty() => {
            Outgoing::Rejected("/html needs some text between the tags".to_owned())
        }
        "html" => {
            let plain = strip_tags(argument).trim().to_owned();
            // Tag stripping changes byte offsets, so fall back to unanchored matching.
            let unanchored: Vec<MentionMark> =
                marks.iter().map(|m| m.clone().with_at(None)).collect();
            let (_, mentions) = resolve_marks(&plain, &unanchored);
            Outgoing::Text {
                plain,
                html: Some(argument.to_owned()),
                mentions: finish_mentions(mentions, preserve_empty_mentions),
            }
        }
        "spoiler" => {
            let (linked, mentions) = resolve_marks(argument, &arg_marks);
            needs_text(Outgoing::Text {
                plain: format!("||{argument}||"),
                html: Some(format!(
                    "<span data-mx-spoiler>{}</span>",
                    fill_emotes(&inline_html(&linked), &arg_marks)
                )),
                mentions: finish_mentions(mentions, preserve_empty_mentions),
            })
        }
        "rainbow" => {
            let (_, mentions) = resolve_marks(argument, &arg_marks);
            needs_text(Outgoing::Text {
                plain: argument.to_owned(),
                html: Some(rainbow(argument)),
                mentions: finish_mentions(mentions, preserve_empty_mentions),
            })
        }
        "code" => needs_text(code_message(argument, preserve_empty_mentions)),
        "shrug" => face_message(
            SHRUG,
            SHRUG_MD,
            argument,
            &arg_marks,
            preserve_empty_mentions,
        ),
        "tableflip" => face_message(
            TABLEFLIP,
            TABLEFLIP,
            argument,
            &arg_marks,
            preserve_empty_mentions,
        ),
        "lenny" => face_message(LENNY, LENNY, argument, &arg_marks, preserve_empty_mentions),
        _ => Outgoing::Rejected(format!(
            "Unknown command /{name}. Start with // to send it as text."
        )),
    }
}

/// A text as a message, with the Markdown of the composer and none of its slash
/// commands: for texts that no person typed here (the agent channel), where a leading
/// slash is the start of a path, not of a command.
#[cfg(any(not(target_family = "wasm"), test))]
pub fn message(text: &str) -> Outgoing {
    markdown_message(text.trim(), &[], false)
}

fn shift_marks(raw: &str, sub: &str, extra_left: usize, marks: &[MentionMark]) -> Vec<MentionMark> {
    if marks.is_empty() || sub.is_empty() {
        return Vec::new();
    }
    let delta = (sub.as_ptr() as usize)
        .saturating_sub(raw.as_ptr() as usize)
        .saturating_sub(extra_left);
    marks.iter().map(|m| m.shifted(delta)).collect()
}

fn prepend(face: &str, text: &str) -> String {
    if text.is_empty() {
        face.to_owned()
    } else {
        format!("{face} {text}")
    }
}

fn finish_mentions(mentions: Mentions, preserve_empty: bool) -> Option<Mentions> {
    if !mentions.user_ids.is_empty() || mentions.room || preserve_empty {
        Some(mentions)
    } else {
        None
    }
}

fn face_message(
    face: &str,
    face_md: &str,
    argument: &str,
    marks: &[MentionMark],
    preserve_empty: bool,
) -> Outgoing {
    let (linked, mentions) = resolve_marks(argument, marks);
    Outgoing::Text {
        plain: prepend(face, argument),
        html: message_html(&prepend(face_md, &linked), marks),
        mentions: finish_mentions(mentions, preserve_empty),
    }
}

fn markdown_message(text: &str, marks: &[MentionMark], preserve_empty: bool) -> Outgoing {
    let tidied = tidy_fences(text);
    // Moving code onto lines of its own moves what follows: marks are then found by
    // their text, not by their place.
    let unanchored: Vec<MentionMark>;
    let (text, marks) = match &tidied {
        std::borrow::Cow::Borrowed(_) => (text, marks),
        std::borrow::Cow::Owned(changed) => {
            unanchored = marks.iter().map(|m| m.clone().with_at(None)).collect();
            (changed.as_str(), unanchored.as_slice())
        }
    };
    let (linked, mentions) = resolve_marks(text, marks);
    Outgoing::Text {
        plain: text.to_owned(),
        html: message_html(&linked, marks),
        mentions: finish_mentions(mentions, preserve_empty),
    }
}

/// The run of three or more backticks a line starts with (after up to three spaces),
/// and what follows it.
fn fence_start(line: &str) -> Option<(usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ticks = rest.len() - rest.trim_start_matches('`').len();
    (ticks >= 3).then(|| (ticks, &rest[ticks..]))
}

/// What people type when they mean a code block is often all on one line:
/// ```` ```bash ls -la``` ````, or the code right after the language. Markdown reads
/// everything after the opening backticks as the block's label and the block as
/// empty: the code would be gone from the message. Such code is moved onto a line of
/// its own (and a closing run at the end of the line onto the next), so that it is
/// the block's content. The first word stays the language only if it is one.
fn tidy_fences(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("```") {
        return text.into();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut changed = false;
    // The length of the run that opened the block we are in.
    let mut open: Option<usize> = None;
    // The length of the run of `~` that opened the block we are in; such a block is
    // left alone as a whole.
    let mut tildes: Option<usize> = None;
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        if open.is_none() {
            let indent = line.len() - line.trim_start_matches(' ').len();
            let after = &line[indent..];
            let run = after.len() - after.trim_start_matches('~').len();
            let fence = indent <= 3 && run >= 3;
            match tildes {
                // Ended only by a bare run that is long enough, as with backticks.
                Some(opened) => {
                    if fence && run >= opened && after[run..].trim().is_empty() {
                        tildes = None;
                    }
                    out.push_str(line);
                    continue;
                }
                None if fence => {
                    tildes = Some(run);
                    out.push_str(line);
                    continue;
                }
                None => {}
            }
        }
        let Some((ticks, rest)) = fence_start(line) else {
            out.push_str(line);
            continue;
        };
        if let Some(opened) = open {
            // Inside a block only a bare run that is long enough ends it.
            if ticks >= opened && rest.trim().is_empty() {
                open = None;
            }
            out.push_str(line);
            continue;
        }
        let fence = &line[..line.len() - rest.len()];
        let label = rest.trim();
        // Closed on the same line?
        let closing = label.len() - label.trim_end_matches('`').len();
        let (inner, closed) = if closing >= ticks {
            (label[..label.len() - closing].trim(), true)
        } else {
            (label, false)
        };
        let (first, more) = match inner.split_once(char::is_whitespace) {
            Some((first, more)) => (first, more.trim()),
            None => (inner, ""),
        };
        let (lang, code) = if more.is_empty() && !closed {
            // A label and nothing else: an ordinary opening line.
            open = Some(ticks);
            out.push_str(line);
            continue;
        } else if !more.is_empty() && crate::highlight::known(first) {
            (first, more)
        } else {
            ("", inner)
        };
        if inner.contains('`') || code.is_empty() {
            // Not something to guess at (an inline piece like ```a``` b ```c```).
            open = (!closed).then_some(ticks);
            out.push_str(line);
            continue;
        }
        changed = true;
        out.push_str(fence);
        out.push_str(lang);
        out.push('\n');
        out.push_str(code);
        if closed {
            out.push('\n');
            out.push_str(&"`".repeat(ticks));
        } else {
            open = Some(ticks);
        }
    }
    if changed { out.into() } else { text.into() }
}

/// `/code [language] text`: a code block without typing any backticks. The first word
/// is the language if it is one and code follows it.
fn code_message(argument: &str, preserve_empty: bool) -> Outgoing {
    // After the language: spaces up to the code on the same line, or one line break.
    // What the code's first line is indented by is the code's.
    let (first, more) = match argument.find(char::is_whitespace) {
        Some(at) => {
            let more = argument[at..].trim_start_matches([' ', '\t']);
            let more = more
                .strip_prefix("\r\n")
                .or_else(|| more.strip_prefix('\n'))
                .unwrap_or(more);
            (&argument[..at], more)
        }
        None => (argument, ""),
    };
    let (lang, code) = if !more.trim().is_empty() && crate::highlight::known(first) {
        (first, more)
    } else {
        ("", argument)
    };
    // A run of backticks longer than any in the code, so that it cannot end the block.
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let class = if lang.is_empty() {
        String::new()
    } else {
        format!(" class=\"language-{}\"", escape(lang))
    };
    Outgoing::Text {
        plain: format!("{fence}{lang}\n{code}\n{fence}"),
        html: Some(format!("<pre><code{class}>{}\n</code></pre>", escape(code))),
        mentions: finish_mentions(Mentions::new(), preserve_empty),
    }
}

/// `|| like this ||` is meant as a spoiler just as `||like this||` is, but Markdown
/// only takes delimiters that hug their text. Spaces just inside a pair of `||` are
/// dropped before parsing. Left alone: code (spans, fenced and indented blocks, also
/// inside quotes), tables, escaped bars, and bars glued to the end of a word (as in an
/// address). As in other chat apps, `a || b || c` does hide the `b`.
fn tighten_spoilers(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("||") {
        return text.into();
    }
    let code = crate::complete::code_mask(text, false);
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    // Lines of a table: every line of a block that has a delimiter row (|---|---|).
    let content = |line: &str| -> String { line.trim().trim_start_matches(['>', ' ']).to_owned() };
    let is_delimiter_row = |line: &str| {
        let row = content(line);
        row.contains('-')
            && row.contains('|')
            && row.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
    };
    let mut in_table = vec![false; lines.len()];
    for (n, line) in lines.iter().enumerate() {
        if is_delimiter_row(line) {
            let blank = |i: usize| content(lines[i]).is_empty();
            let mut first = n;
            while first > 0 && !blank(first - 1) {
                first -= 1;
            }
            let mut last = n;
            while last + 1 < lines.len() && !blank(last + 1) {
                last += 1;
            }
            in_table[first..=last].fill(true);
        }
    }

    let mut out = String::with_capacity(text.len());
    let mut offset = 0;
    let mut fence: Option<char> = None;
    for (n, line) in lines.iter().enumerate() {
        let start = offset;
        offset += line.len();
        // Indented code (outside a fence; inside one the fence decides).
        if fence.is_none() && (line.starts_with("    ") || line.starts_with('\t')) {
            out.push_str(line);
            continue;
        }
        // Fenced code, also when it sits in a quote or a list.
        let inner = content(line);
        let inner = inner.trim_start_matches(['-', '*', '+', ' ']);
        let fence_mark = ['`', '~']
            .into_iter()
            .find(|c| inner.starts_with(&c.to_string().repeat(3)));
        match (fence, fence_mark) {
            (None, Some(mark)) => {
                fence = Some(mark);
                out.push_str(line);
                continue;
            }
            (Some(open), Some(mark)) if open == mark => {
                fence = None;
                out.push_str(line);
                continue;
            }
            (Some(_), _) => {
                out.push_str(line);
                continue;
            }
            (None, None) => {}
        }
        // Rows of a table.
        if in_table[n] {
            out.push_str(line);
            continue;
        }
        let bytes = line.as_bytes();
        let free = |at: usize| !code[start + at] && (at == 0 || bytes[at - 1] != b'\\');
        let marks: Vec<usize> = line
            .match_indices("||")
            .map(|(at, _)| at)
            .filter(|at| free(*at))
            .collect();
        let mut copied = 0;
        let mut k = 0;
        while k + 1 < marks.len() {
            let (open, close) = (marks[k], marks[k + 1]);
            // An opening pair starts a word: bars that end one (an address, `a||`)
            // open nothing.
            let opens = line[..open]
                .chars()
                .next_back()
                .is_none_or(|c| c.is_whitespace() || "([{\"'".contains(c));
            let hidden = line[open + 2..close].trim();
            if !opens || hidden.is_empty() {
                k += 1;
                continue;
            }
            out.push_str(&line[copied..open + 2]);
            out.push_str(hidden);
            copied = close;
            k += 2;
        }
        out.push_str(&line[copied..]);
    }
    out.into()
}

/// Renders Markdown to Matrix HTML. `None` when the text contains no formatting, so
/// plain messages are sent without a redundant HTML copy.
pub fn markdown_html(text: &str) -> Option<String> {
    let (html, formatted) = render_markdown(text);
    formatted.then_some(html)
}

/// The HTML of a message whose Markdown, with accepted completions resolved, is
/// `linked`: like [`markdown_html`], but a message with a custom emoji always has HTML,
/// since that is the only place its picture can be.
fn message_html(linked: &str, marks: &[MentionMark]) -> Option<String> {
    let (html, formatted) = render_markdown(linked);
    if html.contains(EMOTE_OPEN) {
        Some(fill_emotes(&html, marks))
    } else {
        formatted.then_some(html)
    }
}

/// Puts the custom emoji of `marks` in place of the placeholders `resolve_marks` left.
/// Inside a tag (an address the Markdown made a link of) only its text goes in.
fn fill_emotes(html: &str, marks: &[MentionMark]) -> String {
    if !html.contains(EMOTE_OPEN) {
        return html.to_owned();
    }
    let mut out = String::with_capacity(html.len() + 64);
    let mut in_tag = false;
    // Inside code (also the indented kind, which is only known after rendering) a
    // shortcode stays the text it is.
    let mut code_depth = 0usize;
    let mut rest = html;
    while let Some(c) = rest.chars().next() {
        if rest.starts_with("<code") || rest.starts_with("<pre") {
            code_depth += 1;
        } else if rest.starts_with("</code") || rest.starts_with("</pre") {
            code_depth = code_depth.saturating_sub(1);
        }
        if let Some((code, url, len)) = placeholder_at(rest, marks) {
            let code = escape(code);
            if in_tag || code_depth > 0 || !crate::matrix::is_valid_mxc(url) {
                out.push_str(&code);
            } else {
                out.push_str(&format!(
                    "<img data-mx-emoticon src=\"{url}\" alt=\"{code}\" title=\"{code}\" height=\"32\">"
                ));
            }
            rest = &rest[len..];
            continue;
        }
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ => {}
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The custom emoji whose placeholder starts `text`: its `:shortcode:`, its address, and
/// the placeholder's length in bytes.
fn placeholder_at<'m>(text: &str, marks: &'m [MentionMark]) -> Option<(&'m str, &'m str, usize)> {
    let inner = text.strip_prefix(EMOTE_OPEN)?;
    let end = inner.find(EMOTE_CLOSE)?;
    let index: usize = inner[..end].parse().ok()?;
    let MentionKind::Emoticon { code, url } = &marks.get(index)?.kind else {
        return None;
    };
    Some((
        code.as_str(),
        url.as_str(),
        EMOTE_OPEN.len_utf8() + end + EMOTE_CLOSE.len_utf8(),
    ))
}

/// Markdown rendered to HTML, and whether it has any markup beyond paragraphs and
/// line breaks.
fn render_markdown(text: &str) -> (String, bool) {
    let mut options = Options::default();
    options.extension.strikethrough = true;
    options.extension.spoiler = true;
    options.extension.autolink = true;
    options.extension.table = true;
    // In a chat, a line break typed is a line break meant.
    options.render.hardbreaks = true;
    // Raw HTML in the input is shown as text, never passed through.
    options.render.escape = true;

    let text = tighten_spoilers(text);
    let html = comrak::markdown_to_html(&text, &options);
    let mut html = html.trim_end().to_owned();
    // Matrix marks spoilers with an attribute, not a class.
    html = html.replace("<span class=\"spoiler\">", "<span data-mx-spoiler>");
    // A single paragraph needs no wrapper; clients would add a block margin for it.
    if html.matches("<p>").count() == 1
        && let Some(inner) = html
            .strip_prefix("<p>")
            .and_then(|rest| rest.strip_suffix("</p>"))
    {
        html = inner.to_owned();
    }
    // Only paragraphs and line breaks, no other markup: the plain body says it all.
    let mut bare = html.clone();
    for tag in ["<br />\n", "<br />", "</p>\n", "</p>", "<p>"] {
        bare = bare.replace(tag, "");
    }
    let formatted = bare.contains('<');
    (html, formatted)
}

/// HTML for inside an inline element: Markdown formatting, but line and paragraph
/// breaks as `<br />` instead of block tags.
fn inline_html(text: &str) -> String {
    match markdown_html(text) {
        Some(html) => html
            .replace("</p>\n<p>", "<br /><br />")
            .replace("<p>", "")
            .replace("</p>", ""),
        None => escape(text).replace('\n', "<br />"),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A rough plain-text version of HTML, for the fallback body of `/html`.
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            rest = &rest[start..];
            break;
        };
        // Line and paragraph ends become newlines so words do not run together.
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        if tag.starts_with("br") || tag == "/p" || tag == "/div" || tag == "/li" {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

fn rainbow(text: &str) -> String {
    let count = text.chars().filter(|c| !c.is_whitespace()).count().max(1);
    let mut out = String::new();
    let mut n = 0;
    for c in text.chars() {
        if c.is_whitespace() {
            out.push(c);
            continue;
        }
        let (r, g, b) = hue_to_rgb(n as f32 / count as f32);
        n += 1;
        out.push_str(&format!(
            "<span data-mx-color=\"#{r:02x}{g:02x}{b:02x}\">{}</span>",
            escape(c.encode_utf8(&mut [0; 4]))
        ));
    }
    out
}

/// Fully saturated colour for a hue in `0.0..1.0`.
fn hue_to_rgb(hue: f32) -> (u8, u8, u8) {
    let h = hue * 6.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(plain: &str, html: Option<&str>) -> Outgoing {
        Outgoing::Text {
            plain: plain.into(),
            html: html.map(str::to_owned),
            mentions: None,
        }
    }

    #[test]
    fn plain_text_has_no_html() {
        assert_eq!(parse("hello world"), text("hello world", None));
        assert_eq!(parse("  two\nlines  "), text("two\nlines", None));
        assert_eq!(parse("1 < 2 & 3 > 2"), text("1 < 2 & 3 > 2", None));
        // Paragraphs alone are not formatting.
        assert_eq!(parse("first\n\nsecond"), text("first\n\nsecond", None));
    }

    #[test]
    fn markdown_formatting() {
        assert_eq!(
            parse("**bold** and *italic*"),
            text(
                "**bold** and *italic*",
                Some("<strong>bold</strong> and <em>italic</em>")
            )
        );
        assert_eq!(
            parse("~~no~~ `code`"),
            text("~~no~~ `code`", Some("<del>no</del> <code>code</code>"))
        );
        assert_eq!(
            parse("a\n**b**"),
            text("a\n**b**", Some("a<br />\n<strong>b</strong>"))
        );
        let Outgoing::Text {
            html: Some(html), ..
        } = parse("```rust\nfn main() {}\n```")
        else {
            panic!("code block should be formatted");
        };
        assert!(html.contains("<pre><code class=\"language-rust\">fn main() {}"));
        let Outgoing::Text {
            html: Some(html), ..
        } = parse("one\n\ntwo **x**")
        else {
            panic!();
        };
        assert!(html.starts_with("<p>one</p>"), "{html}");
    }

    #[test]
    fn spoilers_may_have_spaces_inside_the_bars() {
        let spoiled = Some("the end: <span data-mx-spoiler>he dies</span>".to_owned());
        for text in [
            "the end: ||he dies||",
            "the end: || he dies||",
            "the end: ||he dies ||",
            "the end: ||  he dies  ||",
        ] {
            assert_eq!(markdown_html(text), spoiled, "{text}");
        }
        // Two on a line, and formatting inside.
        assert_eq!(
            markdown_html("|| a || and || *b* ||").as_deref(),
            Some("<span data-mx-spoiler>a</span> and <span data-mx-spoiler><em>b</em></span>")
        );
        // Left as typed: code of every kind, tables, escaped bars, bars at the end
        // of a word, empty and unpaired bars.
        for text in [
            "`|| a ||`",
            "```\n|| a ||\n```",
            "> ```\n> || a ||\n> ```",
            "- ~~~\n  || a ||\n  ~~~",
            "    || a ||",
            "| x | y |\n|---|---|\n| a || b || c |",
            "x | y\n--|--\na || b || c",
            "\\|| a \\||",
            "see http://example.org/a|| b ||",
            "a || b",
            "||  ||",
        ] {
            assert_eq!(tighten_spoilers(text), text, "{text}");
        }
        // After a fence has closed, and in a quote, it works again.
        assert_eq!(
            tighten_spoilers("```\ncode\n```\n> || a ||"),
            "```\ncode\n```\n> ||a||"
        );
        // As in other chat apps, this hides the middle.
        assert_eq!(tighten_spoilers("a || b || c"), "a ||b|| c");
        // The text sent as plain body stays as typed.
        let Outgoing::Text { plain, html, .. } = parse("|| secret ||") else {
            panic!("not text");
        };
        assert_eq!(plain, "|| secret ||");
        assert_eq!(html.as_deref(), Some("<span data-mx-spoiler>secret</span>"));
    }

    #[test]
    fn spoilers_use_the_matrix_attribute() {
        assert_eq!(
            parse("the end: ||he dies||"),
            text(
                "the end: ||he dies||",
                Some("the end: <span data-mx-spoiler>he dies</span>")
            )
        );
        assert_eq!(
            parse("/spoiler he *dies*"),
            text(
                "||he *dies*||",
                Some("<span data-mx-spoiler>he <em>dies</em></span>")
            )
        );
    }

    #[test]
    fn raw_html_is_escaped_not_passed_through() {
        let Outgoing::Text { html, .. } = parse("**x** <script>alert(1)</script>") else {
            panic!();
        };
        let html = html.unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn code_typed_on_the_opening_line_is_the_blocks_content_not_its_label() {
        let block = |typed: &str| {
            let Outgoing::Text { plain, html, .. } = parse(typed) else {
                panic!("expected text for {typed}");
            };
            (plain, html.unwrap_or_default())
        };
        // All on one line, as it is typed without thinking of line breaks.
        let (plain, html) = block("```bash ls -la /tmp```");
        assert_eq!(plain, "```bash\nls -la /tmp\n```");
        assert!(
            html.contains("<pre><code class=\"language-bash\">ls -la /tmp\n</code></pre>"),
            "{html}"
        );
        // Without a language: the first word is code too.
        let (plain, html) = block("```ls -la /tmp```");
        assert_eq!(plain, "```\nls -la /tmp\n```");
        assert!(
            html.contains("<pre><code>ls -la /tmp\n</code></pre>"),
            "{html}"
        );
        // The code after the language, the rest as usual.
        let (plain, html) = block("```rust fn main() {\n    run();\n}\n```");
        assert_eq!(plain, "```rust\nfn main() {\n    run();\n}\n```");
        assert!(html.contains("language-rust") && html.contains("fn main() {\n    run();\n}"));
        // Text around it stays where it is.
        let (plain, html) = block("look:\n```sh echo hi```\nthat is all");
        assert_eq!(plain, "look:\n```sh\necho hi\n```\nthat is all");
        assert!(
            html.contains("echo hi") && html.contains("that is all"),
            "{html}"
        );
    }

    #[test]
    fn well_formed_blocks_and_inline_pieces_are_left_as_typed() {
        for typed in [
            "```rust\nfn main() {}\n```",
            "```\nplain\n```",
            "```rust\n``` not an end\n```",
            "a `piece` of code",
            "```a``` and ```b```",
            "no code at all",
        ] {
            assert!(
                matches!(tidy_fences(typed), std::borrow::Cow::Borrowed(_)),
                "{typed}"
            );
        }
        // Nor inside a block fenced with tildes.
        let typed = "~~~\n```bash ls -la```\n~~~\nafter";
        assert!(matches!(tidy_fences(typed), std::borrow::Cow::Borrowed(_)));
        // A longer run of tildes is not ended by a shorter one, nor by one with a label.
        let typed = "~~~~\n~~~\n```bash ls -la```\n~~~ x\n```sh echo```\n~~~~";
        assert!(matches!(tidy_fences(typed), std::borrow::Cow::Borrowed(_)));
        // After the block has ended, the repair works again.
        let typed = "~~~\ninside\n~~~\n```sh echo hi```";
        assert_eq!(tidy_fences(typed), "~~~\ninside\n~~~\n```sh\necho hi\n```");
        // Inside a block nothing is touched, whatever it looks like.
        let typed = "```\n```bash ls```\n```";
        assert!(matches!(tidy_fences(typed), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn the_code_command_needs_no_backticks() {
        let block = |typed: &str| {
            let Outgoing::Text { plain, html, .. } = parse(typed) else {
                panic!("expected text for {typed}");
            };
            (plain, html.unwrap_or_default())
        };
        let (plain, html) = block("/code bash ls -la /tmp");
        assert_eq!(plain, "```bash\nls -la /tmp\n```");
        assert_eq!(
            html,
            "<pre><code class=\"language-bash\">ls -la /tmp\n</code></pre>"
        );
        // No language named: all of it is code.
        let (plain, html) = block("/code ls -la /tmp");
        assert_eq!(plain, "```\nls -la /tmp\n```");
        assert_eq!(html, "<pre><code>ls -la /tmp\n</code></pre>");
        // A language alone is code as well (there is nothing else to send).
        assert_eq!(block("/code rust").0, "```\nrust\n```");
        // Several lines, the language on the first.
        let (plain, html) = block("/code rust\nfn main() {\n    a < b;\n}");
        assert_eq!(plain, "```rust\nfn main() {\n    a < b;\n}\n```");
        assert!(
            html.contains("a &lt; b;") && html.contains("language-rust"),
            "{html}"
        );
        // The indentation of the first line of code is kept.
        let (plain, html) = block("/code python\n    return 1\n\n    pass");
        assert_eq!(plain, "```python\n    return 1\n\n    pass\n```");
        assert!(
            html.contains(">    return 1\n\n    pass\n</code>"),
            "{html}"
        );
        // Backticks in the code cannot end the block.
        let (plain, _) = block("/code echo ```hi```");
        assert!(
            plain.starts_with("````\n") && plain.ends_with("\n````"),
            "{plain}"
        );
        assert!(matches!(parse("/code"), Outgoing::Rejected(_)));
    }

    #[test]
    fn slash_commands() {
        assert_eq!(
            parse("/me waves *hard*"),
            Outgoing::Emote {
                plain: "waves *hard*".into(),
                html: Some("waves <em>hard</em>".into()),
                mentions: None,
            }
        );
        assert_eq!(parse("/plain **not bold**"), text("**not bold**", None));
        assert_eq!(
            parse("/html <b>x</b> &amp; y"),
            text("x & y", Some("<b>x</b> &amp; y"))
        );
        assert_eq!(parse("/shrug"), text(SHRUG, None));
        assert_eq!(
            parse("/shrug oh well"),
            text(&format!("{SHRUG} oh well"), None)
        );
        // When HTML is emitted (formatting or autolink in the rest), /shrug keeps its
        // backslash and its underscores never pair into emphasis with the rest.
        assert_eq!(
            parse("/shrug **bold**"),
            text(
                &format!("{SHRUG} **bold**"),
                Some(&format!("{SHRUG} <strong>bold</strong>"))
            )
        );
        assert_eq!(
            parse("/shrug see https://example.org"),
            text(
                &format!("{SHRUG} see https://example.org"),
                Some(&format!(
                    "{SHRUG} see <a href=\"https://example.org\">https://example.org</a>"
                ))
            )
        );
        assert_eq!(
            parse("/shrug check snake_case"),
            text(&format!("{SHRUG} check snake_case"), None)
        );
        assert_eq!(
            parse("/shrug _one_ and _two"),
            text(
                &format!("{SHRUG} _one_ and _two"),
                Some(&format!("{SHRUG} <em>one</em> and _two"))
            )
        );
        assert_eq!(parse("//not a command"), text("/not a command", None));
        assert!(matches!(parse("/me"), Outgoing::Rejected(_)));
        assert!(matches!(parse("/html <b>  </b>"), Outgoing::Rejected(_)));
        assert_eq!(
            parse("/html <p>one</p><p>two<br>three</p>"),
            text("one\ntwo\nthree", Some("<p>one</p><p>two<br>three</p>"))
        );
        // Spoilers stay inline whatever is inside them.
        assert_eq!(
            parse("/spoiler line 1\nline 2"),
            text(
                "||line 1\nline 2||",
                Some("<span data-mx-spoiler>line 1<br />line 2</span>")
            )
        );
        let Outgoing::Text {
            html: Some(html), ..
        } = parse("/spoiler one\n\n**two**")
        else {
            panic!();
        };
        assert_eq!(
            html,
            "<span data-mx-spoiler>one<br /><br /><strong>two</strong></span>"
        );
        assert!(matches!(parse("/nosuchthing x"), Outgoing::Rejected(_)));
        let Outgoing::Text { plain, html, .. } = parse("/rainbow hi <") else {
            panic!();
        };
        assert_eq!(plain, "hi <");
        let html = html.unwrap();
        assert_eq!(html.matches("data-mx-color").count(), 3);
        assert!(html.contains("&lt;") && html.contains("#ff0000"));
    }

    #[test]
    fn mentions_render_matrix_to_links_and_populate_m_mentions() {
        let marks = vec![
            MentionMark::user("Alice", "@alice:hs"),
            MentionMark::room_link("#rust:hs", "https://matrix.to/#/%23rust:hs"),
        ];
        let Outgoing::Text {
            plain,
            html,
            mentions,
        } = parse_with_mentions("Hey Alice, check #rust:hs!", &marks, false)
        else {
            panic!("expected text");
        };
        assert_eq!(plain, "Hey Alice, check #rust:hs!");
        assert_eq!(
            html.as_deref(),
            Some(
                "Hey <a href=\"https://matrix.to/#/@alice:hs\">Alice</a>, check <a href=\"https://matrix.to/#/%23rust:hs\">#rust:hs</a>!"
            )
        );
        let mentions = mentions.expect("expected m.mentions");
        assert!(mentions.user_ids.iter().any(|u| u.as_str() == "@alice:hs"));
        assert!(!mentions.room);

        // /me and /spoiler keep plain body untouched while HTML gets matrix.to links.
        let Outgoing::Emote {
            plain: emote_plain,
            html: emote_html,
            mentions: emote_mentions,
        } = parse_with_mentions("/me waves to Alice", &marks, false)
        else {
            panic!("expected emote");
        };
        assert_eq!(emote_plain, "waves to Alice");
        assert!(
            emote_html
                .unwrap()
                .contains("href=\"https://matrix.to/#/@alice:hs\"")
        );
        assert!(emote_mentions.is_some());

        // Editing away all mentions emits empty `Some(Mentions::new())` when preserving.
        let Outgoing::Text {
            mentions: cleared, ..
        } = parse_with_mentions("Never mind", &marks, true)
        else {
            panic!("expected text");
        };
        let cleared = cleared.expect("should preserve empty mentions when editing");
        assert!(cleared.user_ids.is_empty() && !cleared.room);
    }

    #[test]
    fn a_chosen_custom_emoji_is_sent_as_a_picture_with_its_shortcode_as_text() {
        let blob = "<img data-mx-emoticon src=\"mxc://example.org/blob\" alt=\":blob:\" \
                    title=\":blob:\" height=\"32\">";
        let marks = vec![MentionMark::emoticon("blob", "mxc://example.org/blob").with_at(Some(3))];
        // No other formatting: the HTML is there for the picture alone.
        assert_eq!(
            parse_with_mentions("hi :blob: there", &marks, false),
            text("hi :blob: there", Some(&format!("hi {blob} there")))
        );
        // With Markdown around it, and in an action.
        let Outgoing::Text { html, .. } = parse_with_mentions("hi :blob: **yes**", &marks, false)
        else {
            panic!("expected text");
        };
        assert_eq!(
            html.as_deref(),
            Some(format!("hi {blob} <strong>yes</strong>").as_str())
        );
        let me = vec![MentionMark::emoticon("blob", "mxc://example.org/blob")];
        let Outgoing::Emote { plain, html, .. } = parse_with_mentions("/me :blob:", &me, false)
        else {
            panic!("expected emote");
        };
        assert_eq!(plain, ":blob:");
        assert_eq!(html.as_deref(), Some(blob));
        let Outgoing::Text { html, .. } = parse_with_mentions("/spoiler :blob:", &me, false) else {
            panic!("expected text");
        };
        assert_eq!(
            html.as_deref(),
            Some(format!("<span data-mx-spoiler>{blob}</span>").as_str())
        );
        // `/plain` stays plain.
        assert_eq!(
            parse_with_mentions("/plain :blob:", &me, false),
            text(":blob:", None)
        );
    }

    #[test]
    fn a_custom_emoji_shortcode_stays_text_in_code_in_a_word_and_in_an_address() {
        let marks = vec![MentionMark::emoticon("blob", "mxc://example.org/blob")];
        for typed in ["`:blob:`", "x:blob:", ":blob:s"] {
            let Outgoing::Text { html, .. } = parse_with_mentions(typed, &marks, false) else {
                panic!("expected text");
            };
            assert!(!html.unwrap_or_default().contains("<img"), "{typed}");
        }
        // An indented code block is only code once the Markdown is rendered.
        let Outgoing::Text { html, .. } =
            parse_with_mentions("text\n\n    :blob: in code\n", &marks, false)
        else {
            panic!("expected text");
        };
        let html = html.unwrap();
        assert!(html.contains("<code>:blob: in code"), "{html}");
        assert!(
            !html.contains("<img") && !html.contains('\u{e000}'),
            "{html}"
        );
        // Made a link by the Markdown: the picture cannot go inside the address.
        let Outgoing::Text { html, .. } =
            parse_with_mentions("https://example.org/:blob: ok", &marks, false)
        else {
            panic!("expected text");
        };
        let html = html.unwrap();
        assert!(
            html.contains("<a href=\"https://example.org/:blob"),
            "{html}"
        );
        assert!(
            !html.contains("<img") && !html.contains('\u{e000}'),
            "{html}"
        );
    }

    #[test]
    fn a_custom_emoji_with_a_bad_address_or_name_cannot_break_the_html() {
        let marks = vec![
            MentionMark::emoticon("a\"b<c>", "mxc://example.org/ok"),
            MentionMark::emoticon("evil", "mxc://example.org/x\" onerror=\"y"),
        ];
        let Outgoing::Text { plain, html, .. } =
            parse_with_mentions(":a\"b<c>: :evil:", &marks, false)
        else {
            panic!("expected text");
        };
        assert_eq!(plain, ":a\"b<c>: :evil:");
        let html = html.unwrap();
        assert!(html.contains("alt=\":a&quot;b&lt;c&gt;:\""), "{html}");
        assert!(!html.contains("onerror"), "{html}");
        assert!(html.ends_with(" :evil:"), "{html}");
    }
}
