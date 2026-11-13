//! Turning what the user typed into a Matrix message: Markdown, plus slash commands.
//!
//! Matrix carries formatting as HTML next to a plain-text body; bridges translate that
//! HTML into each network's own formatting.

use comrak::Options;
use matrix_sdk::ruma::events::Mentions;

use crate::complete::{MentionMark, resolve_marks};

/// What to do with a line of input.
#[derive(Debug, Clone)]
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
                html: markdown_html(&linked),
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
                    inline_html(&linked)
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
        "shrug" => face_message(SHRUG, argument, &arg_marks, preserve_empty_mentions),
        "tableflip" => face_message(TABLEFLIP, argument, &arg_marks, preserve_empty_mentions),
        "lenny" => face_message(LENNY, argument, &arg_marks, preserve_empty_mentions),
        _ => Outgoing::Rejected(format!(
            "Unknown command /{name}. Start with // to send it as text."
        )),
    }
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
    argument: &str,
    marks: &[MentionMark],
    preserve_empty: bool,
) -> Outgoing {
    let (linked, mentions) = resolve_marks(argument, marks);
    Outgoing::Text {
        plain: prepend(face, argument),
        html: markdown_html(&prepend(face, &linked)),
        mentions: finish_mentions(mentions, preserve_empty),
    }
}

fn markdown_message(text: &str, marks: &[MentionMark], preserve_empty: bool) -> Outgoing {
    let (linked, mentions) = resolve_marks(text, marks);
    Outgoing::Text {
        plain: text.to_owned(),
        html: markdown_html(&linked),
        mentions: finish_mentions(mentions, preserve_empty),
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
        // Indented code, and rows of a table.
        let indented = line.starts_with("    ") || line.starts_with('\t');
        if indented || in_table[n] {
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
    if bare.contains('<') { Some(html) } else { None }
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
}
