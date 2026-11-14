//! Formatted message text: parses the HTML subset Matrix messages use into styled
//! lines, and wraps them to a width.
//!
//! This is a display parser, not a browser: unknown tags are ignored (their text is
//! kept), and nothing from the input is ever interpreted beyond styling.

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{sanitize, width};

/// How a piece of text should look. Resolved to colours by the theme at draw time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Format {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub spoiler: bool,
    /// Index into [`Rich::links`].
    pub link: Option<usize>,
    /// `data-mx-color`, as `#rrggbb`.
    pub color: Option<(u8, u8, u8)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub format: Format,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RichLine {
    /// Quote bars and list markers; repeated (as spaces for markers) on wrapped rows.
    pub prefix: String,
    /// The prefix for continuation rows.
    pub hanging: String,
    pub segments: Vec<Segment>,
    /// Code blocks keep their spacing and are cut, not re-flowed.
    pub preformatted: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rich {
    pub lines: Vec<RichLine>,
    pub links: Vec<String>,
}

impl Rich {
    /// Plain text: one line per line, no styling, but web and `matrix:` addresses
    /// become links.
    pub fn plain(text: &str) -> Rich {
        let text = sanitize(text);
        let mut rich = Rich::default();
        for line in text.split('\n') {
            let mut segments = Vec::new();
            let mut rest = line;
            while let Some((start, scheme_len)) = find_plain_link(rest) {
                let tail = &rest[start..];
                let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
                // Punctuation that ends the sentence is not part of the address.
                let url =
                    tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'', '"']);
                if url.len() <= scheme_len {
                    // Just the scheme: not a link.
                    let cut = start + end.max(1);
                    segments.push(Segment {
                        text: rest[..cut].to_owned(),
                        format: Format::default(),
                    });
                    rest = &rest[cut..];
                    continue;
                }
                if start > 0 {
                    segments.push(Segment {
                        text: rest[..start].to_owned(),
                        format: Format::default(),
                    });
                }
                segments.push(Segment {
                    text: url.to_owned(),
                    format: Format {
                        link: Some(rich.links.len()),
                        ..Format::default()
                    },
                });
                rich.links.push(url.to_owned());
                rest = &tail[url.len()..];
            }
            if !rest.is_empty() || segments.is_empty() {
                segments.push(Segment {
                    text: rest.to_owned(),
                    format: Format::default(),
                });
            }
            rich.lines.push(RichLine {
                segments,
                ..RichLine::default()
            });
        }
        rich
    }

    /// A message's text as it should be shown: its formatted version if it has one.
    pub fn of(html: Option<&str>, body: &str) -> Rich {
        match html {
            Some(html) => from_html(html),
            None => Rich::plain(body),
        }
    }
}

fn find_plain_link(rest: &str) -> Option<(usize, usize)> {
    let web = ["https://", "http://"]
        .into_iter()
        .filter_map(|scheme| rest.find(scheme).map(|pos| (pos, scheme.len())))
        .min_by_key(|&(pos, _)| pos);
    let matrix = rest
        .match_indices("matrix:")
        .find(|&(pos, _)| {
            pos == 0
                || !rest[..pos]
                    .ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == ':')
        })
        .map(|(pos, s)| (pos, s.len()));
    match (web, matrix) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Open(String, &'a str),
    Close(String),
    Text(&'a str),
}

fn tokenize(html: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        let Some(start) = rest.find('<') else {
            tokens.push(Token::Text(rest));
            break;
        };
        // A `<` that does not start a tag is text.
        let after = &rest[start + 1..];
        let is_tag = after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/' || c == '!');
        let Some(end) = after.find('>').filter(|_| is_tag) else {
            let cut = start + 1;
            tokens.push(Token::Text(&rest[..cut]));
            rest = &rest[cut..];
            continue;
        };
        if start > 0 {
            tokens.push(Token::Text(&rest[..start]));
        }
        let inside = &after[..end];
        rest = &after[end + 1..];
        if inside.starts_with('!') {
            continue;
        }
        if let Some(name) = inside.strip_prefix('/') {
            tokens.push(Token::Close(name.trim().to_ascii_lowercase()));
            continue;
        }
        let inside = inside.trim_end_matches('/');
        let (name, attributes) = match inside.find(char::is_whitespace) {
            Some(at) => (&inside[..at], &inside[at..]),
            None => (inside, ""),
        };
        tokens.push(Token::Open(name.to_ascii_lowercase(), attributes));
    }
    tokens
}

/// The value of `name` in a tag's attribute text; `Some("")` for a bare attribute.
fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attributes;
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let key_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();
        let value = if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            match after.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    let body = &after[1..];
                    let end = body.find(quote).unwrap_or(body.len());
                    rest = body.get(end + 1..).unwrap_or("");
                    &body[..end]
                }
                _ => {
                    let end = after.find(char::is_whitespace).unwrap_or(after.len());
                    rest = &after[end..];
                    &after[..end]
                }
            }
        } else {
            ""
        };
        if key.eq_ignore_ascii_case(name) {
            return Some(value);
        }
    }
}

pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..].find(';').map(|e| e + 1).filter(|e| *e <= 10);
        let decoded = end.and_then(|end| {
            let name = &rest[1..end];
            let ch = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let code = match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => name.strip_prefix('#')?.parse().ok()?,
                    };
                    char::from_u32(code).filter(|c| !c.is_control())?
                }
            };
            Some((ch, end))
        });
        match decoded {
            Some((ch, end)) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_color(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let part = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some((part(0)?, part(2)?, part(4)?))
}

#[derive(Debug)]
enum ListKind {
    Bullet,
    Numbered(u32),
}

struct Builder {
    rich: Rich,
    current: RichLine,
    /// Whether `current` has been started (it may still be empty).
    open: bool,
    formats: Vec<(String, Format)>,
    quotes: usize,
    lists: Vec<ListKind>,
    /// A list marker waiting for the item's first line.
    marker: Option<String>,
    pre: bool,
    /// Depth inside `<mx-reply>`, whose content repeats the quoted message.
    skip: usize,
}

impl Builder {
    fn format(&self) -> Format {
        self.formats
            .last()
            .map(|(_, f)| f.clone())
            .unwrap_or_default()
    }

    fn indent(&self) -> String {
        // Deep nesting is capped so the text always keeps room on the row.
        const MAX_DEPTH: usize = 4;
        format!(
            "{}{}",
            "┃ ".repeat(self.quotes.min(MAX_DEPTH)),
            "  ".repeat(self.lists.len().saturating_sub(1).min(MAX_DEPTH))
        )
    }

    fn start_line(&mut self) {
        if self.open {
            return;
        }
        let indent = self.indent();
        let (prefix, hanging) = match self.marker.take() {
            Some(marker) => (
                format!("{indent}{marker}"),
                format!("{indent}{}", " ".repeat(width(&marker))),
            ),
            None if !self.lists.is_empty() => {
                // Further paragraphs of a list item line up under its text.
                let pad = format!("{indent}  ");
                (pad.clone(), pad)
            }
            None => (indent.clone(), indent),
        };
        self.current = RichLine {
            prefix,
            hanging,
            segments: Vec::new(),
            preformatted: self.pre,
        };
        self.open = true;
    }

    fn end_line(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        let mut line = std::mem::take(&mut self.current);
        if !line.preformatted
            && let Some(last) = line.segments.last_mut()
        {
            last.text.truncate(last.text.trim_end().len());
        }
        line.segments.retain(|s| !s.text.is_empty());
        self.rich.lines.push(line);
    }

    /// Ends the current line if it has content; block elements start on a fresh line.
    fn block_break(&mut self) {
        if self.open
            && self
                .current
                .segments
                .iter()
                .any(|s| !s.text.trim().is_empty())
        {
            self.end_line();
        } else {
            self.open = false;
        }
    }

    fn push_text(&mut self, text: &str) {
        let format = self.format();
        if self.pre {
            let mut lines = text.split('\n');
            if let Some(first) = lines.next() {
                self.push_piece(first, &format);
            }
            for line in lines {
                self.start_line();
                self.end_line();
                self.push_piece(line, &format);
            }
            return;
        }
        // Collapse whitespace like a browser would.
        let mut collapsed = String::with_capacity(text.len());
        let mut pending_space = false;
        for c in text.chars() {
            if c.is_whitespace() {
                pending_space = true;
            } else {
                if pending_space {
                    collapsed.push(' ');
                    pending_space = false;
                }
                collapsed.push(c);
            }
        }
        if pending_space {
            collapsed.push(' ');
        }
        let at_line_start = !self.open || self.current.segments.is_empty();
        let ends_with_space = self
            .current
            .segments
            .last()
            .is_some_and(|s| s.text.ends_with(' '));
        let piece = if at_line_start || ends_with_space {
            collapsed.trim_start()
        } else {
            collapsed.as_str()
        };
        self.push_piece(piece, &format);
    }

    fn push_piece(&mut self, text: &str, format: &Format) {
        if text.is_empty() {
            return;
        }
        self.start_line();
        match self.current.segments.last_mut() {
            Some(last) if last.format == *format => last.text.push_str(text),
            _ => self.current.segments.push(Segment {
                text: text.to_owned(),
                format: format.clone(),
            }),
        }
    }

    fn open_tag(&mut self, name: &str, attributes: &str) {
        let mut format = self.format();
        match name {
            "mx-reply" => {
                self.skip += 1;
                return;
            }
            "br" => {
                self.start_line();
                self.end_line();
                return;
            }
            "hr" => {
                self.block_break();
                self.push_piece("───", &Format::default());
                self.end_line();
                return;
            }
            "p" | "div" | "tr" => self.block_break(),
            "td" | "th" => {
                // Table cells on one row, separated.
                if self.open && !self.current.segments.is_empty() {
                    self.push_piece(" │ ", &Format::default());
                }
                if name == "th" {
                    format.bold = true;
                }
            }
            "blockquote" => {
                self.block_break();
                self.quotes += 1;
            }
            "ul" => {
                self.block_break();
                self.lists.push(ListKind::Bullet);
            }
            "ol" => {
                self.block_break();
                let start = attribute(attributes, "start")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
                self.lists.push(ListKind::Numbered(start));
            }
            "li" => {
                self.block_break();
                self.marker = Some(match self.lists.last_mut() {
                    Some(ListKind::Numbered(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                });
            }
            "pre" => {
                self.block_break();
                self.pre = true;
                format.code = true;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block_break();
                format.bold = true;
            }
            "b" | "strong" => format.bold = true,
            "i" | "em" => format.italic = true,
            "u" | "ins" => format.underline = true,
            "s" | "del" | "strike" => format.strike = true,
            "code" | "tt" => format.code = true,
            "a" => {
                if let Some(href) = attribute(attributes, "href") {
                    let href = sanitize(&decode_entities(href)).trim().to_owned();
                    // Only addresses that are safe to hand to the system opener.
                    let safe = ["https://", "http://", "mailto:", "matrix:"]
                        .iter()
                        .any(|scheme| href.starts_with(scheme))
                        && !href.contains(char::is_whitespace);
                    if safe {
                        format.link = Some(self.rich.links.len());
                        self.rich.links.push(href);
                    }
                }
            }
            "span" | "font" => {
                if attribute(attributes, "data-mx-spoiler").is_some() {
                    format.spoiler = true;
                }
                let color = attribute(attributes, "data-mx-color")
                    .or_else(|| attribute(attributes, "color"));
                if let Some(color) = color.and_then(parse_color) {
                    format.color = Some(color);
                }
            }
            _ => {}
        }
        self.formats.push((name.to_owned(), format));
    }

    fn close_tag(&mut self, name: &str) {
        if name == "mx-reply" {
            self.skip = self.skip.saturating_sub(1);
            return;
        }
        // Tolerate unbalanced markup: close up to the matching open tag, if any.
        if let Some(at) = self.formats.iter().rposition(|(open, _)| open == name) {
            self.formats.truncate(at);
        }
        match name {
            "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block_break()
            }
            "blockquote" => {
                self.block_break();
                self.quotes = self.quotes.saturating_sub(1);
            }
            "ul" | "ol" => {
                self.block_break();
                self.lists.pop();
                self.marker = None;
            }
            "pre" => {
                // A code block usually ends with a newline that is not a line of code.
                if self.open && self.current.segments.is_empty() {
                    self.open = false;
                } else {
                    self.end_line();
                }
                self.pre = false;
            }
            _ => {}
        }
    }
}

/// Parses a Matrix `formatted_body`.
pub fn from_html(html: &str) -> Rich {
    let mut builder = Builder {
        rich: Rich::default(),
        current: RichLine::default(),
        open: false,
        formats: Vec::new(),
        quotes: 0,
        lists: Vec::new(),
        marker: None,
        pre: false,
        skip: 0,
    };
    for token in tokenize(html) {
        match token {
            Token::Open(name, attributes) => {
                if builder.skip > 0 && name != "mx-reply" {
                    continue;
                }
                builder.open_tag(&name, attributes);
            }
            Token::Close(name) => {
                if builder.skip > 0 && name != "mx-reply" {
                    continue;
                }
                builder.close_tag(&name);
            }
            Token::Text(text) => {
                if builder.skip == 0 {
                    builder.push_text(&sanitize(&decode_entities(text)));
                }
            }
        }
    }
    builder.end_line();
    if builder.rich.lines.is_empty() {
        builder.rich.lines.push(RichLine::default());
    }
    builder.rich
}

/// One display row: its prefix and styled pieces.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WrappedRow {
    pub prefix: String,
    pub segments: Vec<Segment>,
}

/// Wraps to `max` columns, breaking at word boundaries and keeping each line's prefix.
pub fn wrap(rich: &Rich, max: usize) -> Vec<WrappedRow> {
    let max = max.max(1);
    let mut rows = Vec::new();
    for line in &rich.lines {
        let room = max.saturating_sub(width(&line.prefix)).max(1);
        let mut row = WrappedRow {
            prefix: line.prefix.clone(),
            segments: Vec::new(),
        };
        let mut used = 0;
        let next_row = |row: &mut WrappedRow, rows: &mut Vec<WrappedRow>, used: &mut usize| {
            let mut finished = std::mem::replace(
                row,
                WrappedRow {
                    prefix: line.hanging.clone(),
                    segments: Vec::new(),
                },
            );
            // A space that fitted before the wrap point is not shown at the row's end.
            if !line.preformatted
                && let Some(last) = finished.segments.last_mut()
            {
                last.text.truncate(last.text.trim_end().len());
            }
            rows.push(finished);
            *used = 0;
        };
        let push = |row: &mut WrappedRow, text: &str, format: &Format| match row.segments.last_mut()
        {
            Some(last) if last.format == *format => last.text.push_str(text),
            _ => row.segments.push(Segment {
                text: text.to_owned(),
                format: format.clone(),
            }),
        };

        for segment in &line.segments {
            if line.preformatted {
                // Code is cut at the edge rather than re-flowed.
                for g in segment.text.graphemes(true) {
                    let gw = width(g);
                    if used + gw > room && used > 0 {
                        next_row(&mut row, &mut rows, &mut used);
                    }
                    push(&mut row, g, &segment.format);
                    used += gw;
                }
                continue;
            }
            for word in segment.text.split_word_bounds() {
                let ww = width(word);
                if used + ww <= room {
                    push(&mut row, word, &segment.format);
                    used += ww;
                } else if word.trim().is_empty() {
                    // Whitespace at a wrap point is dropped.
                    next_row(&mut row, &mut rows, &mut used);
                } else if ww <= room {
                    next_row(&mut row, &mut rows, &mut used);
                    push(&mut row, word, &segment.format);
                    used = ww;
                } else {
                    for g in word.graphemes(true) {
                        let gw = width(g);
                        if used + gw > room && used > 0 {
                            next_row(&mut row, &mut rows, &mut used);
                        }
                        push(&mut row, g, &segment.format);
                        used += gw;
                    }
                }
            }
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rich: &Rich) -> Vec<String> {
        rich.lines
            .iter()
            .map(|l| {
                format!(
                    "{}{}",
                    l.prefix,
                    l.segments
                        .iter()
                        .map(|s| s.text.as_str())
                        .collect::<String>()
                )
            })
            .collect()
    }

    fn rows(html: &str, max: usize) -> Vec<String> {
        wrap(&from_html(html), max)
            .iter()
            .map(|r| {
                format!(
                    "{}{}",
                    r.prefix,
                    r.segments
                        .iter()
                        .map(|s| s.text.as_str())
                        .collect::<String>()
                )
            })
            .collect()
    }

    #[test]
    fn inline_formatting() {
        let rich = from_html(
            "plain <strong>bold <em>both</em></strong> <del>gone</del> <code>x&lt;y</code>",
        );
        assert_eq!(texts(&rich), ["plain bold both gone x<y"]);
        let s = &rich.lines[0].segments;
        assert!(!s[0].format.bold);
        assert!(s[1].format.bold && !s[1].format.italic);
        assert!(s[2].format.bold && s[2].format.italic);
        assert!(s.iter().any(|x| x.format.strike && x.text == "gone"));
        assert!(s.last().unwrap().format.code);
    }

    #[test]
    fn blocks_breaks_and_whitespace() {
        assert_eq!(
            texts(&from_html("<p>one</p>\n<p>two<br>three</p>")),
            ["one", "two", "three"]
        );
        assert_eq!(texts(&from_html("a  \n  b")), ["a b"]);
        assert_eq!(texts(&from_html("a<br/><br/>b")), ["a", "", "b"]);
        assert_eq!(texts(&from_html("")), [""]);
        assert_eq!(texts(&from_html("<h1>Title</h1>text")), ["Title", "text"]);
    }

    #[test]
    fn quotes_and_lists() {
        assert_eq!(
            texts(&from_html(
                "<blockquote><p>quoted</p><blockquote>deep</blockquote></blockquote>after"
            )),
            ["┃ quoted", "┃ ┃ deep", "after"]
        );
        assert_eq!(
            texts(&from_html(
                "<ul><li>a</li><li>b<ul><li>nested</li></ul></li></ul><ol start=\"3\"><li>x</li><li>y</li></ol>"
            )),
            ["• a", "• b", "  • nested", "3. x", "4. y"]
        );
    }

    #[test]
    fn code_blocks_keep_their_layout() {
        let rich = from_html(
            "<pre><code class=\"language-rust\">fn main() {\n    let x = 1;\n}\n</code></pre>",
        );
        assert_eq!(texts(&rich), ["fn main() {", "    let x = 1;", "}"]);
        assert!(rich.lines.iter().all(|l| l.preformatted));
        assert!(rich.lines[1].segments[0].format.code);
    }

    #[test]
    fn links_spoilers_and_colours() {
        let rich = from_html(
            "<a href=\"https://example.org/?a=1&amp;b=2\">site</a> <a href=\"javascript:alert(1)\">bad</a> \
             <span data-mx-spoiler>secret</span> <span data-mx-color=\"#ff0000\">red</span>",
        );
        assert_eq!(rich.links, ["https://example.org/?a=1&b=2"]);
        let s = &rich.lines[0].segments;
        assert_eq!(s[0].format.link, Some(0));
        // An unsafe address is shown as text but is not a link.
        assert!(
            s.iter()
                .any(|x| x.text.contains("bad") && x.format.link.is_none())
        );
        assert!(s.iter().any(|x| x.text == "secret" && x.format.spoiler));
        assert!(
            s.iter()
                .any(|x| x.text == "red" && x.format.color == Some((255, 0, 0)))
        );
    }

    #[test]
    fn reply_fallback_is_dropped() {
        let html = "<mx-reply><blockquote><a href=\"https://matrix.to/#/!r/$e\">In reply to</a> <b>x</b><br>old</blockquote></mx-reply>new text";
        let rich = from_html(html);
        assert_eq!(texts(&rich), ["new text"]);
        assert!(rich.links.is_empty());
    }

    #[test]
    fn malformed_input_never_panics_and_keeps_text() {
        for html in [
            "a < b and c > d",
            "<b>unclosed",
            "</b>stray close",
            "<",
            "<<<>>>",
            "<a href=>x</a>",
            "<span data-mx-color=\"#zzzzzz\">x</span>",
            "&unknown; &#xZZ; &#99999999; &amp",
            "<p><ul><li>a</p></ul></li>",
            "<pre>",
            "tab\there",
        ] {
            let rich = from_html(html);
            let _ = wrap(&rich, 5);
            let _ = wrap(&rich, 1);
        }
        assert_eq!(texts(&from_html("a < b and c > d")), ["a < b and c > d"]);
        assert_eq!(texts(&from_html("<b>unclosed")), ["unclosed"]);
        assert_eq!(
            decode_entities("&unknown; &amp; &#65;&#x42;"),
            "&unknown; & AB"
        );
    }

    #[test]
    fn wrapping_keeps_prefixes_and_widths() {
        assert_eq!(
            rows("<blockquote>one two three four</blockquote>", 11),
            ["┃ one two", "┃ three", "┃ four"]
        );
        assert_eq!(
            rows("<ol><li>alpha beta gamma</li></ol>", 10),
            ["1. alpha", "   beta", "   gamma"]
        );
        assert_eq!(rows("<pre>abcdefgh</pre>", 4), ["abcd", "efgh"]);
        for max in 1..20 {
            for row in wrap(
                &from_html(
                    "<blockquote>日本語 <b>bold</b> wörds 🐀 and <code>code</code></blockquote>",
                ),
                max,
            ) {
                let total =
                    width(&row.prefix) + row.segments.iter().map(|s| width(&s.text)).sum::<usize>();
                assert!(total <= max.max(4), "{max}: {row:?}");
            }
        }
    }

    #[test]
    fn control_characters_never_reach_the_screen() {
        for rich in [
            from_html("<b>a\u{1b}[2Jb</b>&#27;&#x1b;c\u{9b}d\u{202e}e<pre>x\ty\u{1b}[0m\rz</pre>"),
            Rich::plain("a\u{1b}[2Jb\u{7}\tc\u{202e}d"),
        ] {
            for line in &rich.lines {
                for segment in &line.segments {
                    assert!(
                        !segment
                            .text
                            .contains(|c: char| c.is_control() || c == '\u{202e}'),
                        "{segment:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn plain_text_passes_through() {
        let rich = Rich::plain("a <b> &amp;\nsecond");
        assert_eq!(texts(&rich), ["a <b> &amp;", "second"]);
        assert!(rich.links.is_empty());
    }

    #[test]
    fn plain_text_addresses_become_links() {
        let rich = Rich::plain(
            "see https://example.org/a?b=1, or (http://x.test). https:// alone\nhttps://last.example",
        );
        assert_eq!(
            rich.links,
            [
                "https://example.org/a?b=1",
                "http://x.test",
                "https://last.example"
            ]
        );
        assert_eq!(
            texts(&rich),
            [
                "see https://example.org/a?b=1, or (http://x.test). https:// alone",
                "https://last.example"
            ]
        );
        let linked: Vec<&str> = rich.lines[0]
            .segments
            .iter()
            .filter(|s| s.format.link.is_some())
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(linked, ["https://example.org/a?b=1", "http://x.test"]);

        let matrix = Rich::plain(
            "foo_matrix:skip (matrix:u/bob:example.org?action=chat). matrix: alone\nsee matrix:r/rust:matrix.org!",
        );
        assert_eq!(
            matrix.links,
            [
                "matrix:u/bob:example.org?action=chat",
                "matrix:r/rust:matrix.org"
            ]
        );
    }

    #[test]
    fn tables_and_deep_nesting() {
        assert_eq!(
            texts(&from_html(
                "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>"
            )),
            ["a │ b", "1 │ 2"]
        );
        let deep = format!(
            "{}x{}",
            "<blockquote>".repeat(100),
            "</blockquote>".repeat(100)
        );
        for row in wrap(&from_html(&deep), 20) {
            assert!(width(&row.prefix) <= 8, "{row:?}");
        }
        let rich = from_html(
            "<a href=\" https://ok.example \">x</a><a href=\"https://bad example\">y</a>",
        );
        assert_eq!(rich.links, ["https://ok.example"]);
    }
}
