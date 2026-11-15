//! Formatted message text: parses the HTML subset Matrix messages use into styled
//! lines, and wraps them to a width.
//!
//! This is a display parser, not a browser: unknown tags are ignored (their text is
//! kept), and nothing from the input is ever interpreted beyond styling.

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{is_blank, sanitize, sanitize_line, truncate, unsafe_char, width};

/// Stands in for a custom emoji whose name is not known.
pub const EMOTICON_PLACEHOLDER: &str = ":?:";

/// What a custom emoji whose picture is shown takes in a wrapped row: two cells, the
/// width of an ordinary emoji, which the picture is painted over when the row is drawn.
pub const EMOTE_BOX: &str = "  ";

/// A custom emoji as text: its `:shortcode:` from the `alt` or `title` the sender gave,
/// cleaned like all message text and kept short.
/// Whether an `<img>` is a custom emoji. The mark for it (`data-mx-emoticon`) does not
/// survive the SDK's cleaning of received HTML, so a name of the form `:shortcode:` in
/// `alt` or `title` counts as well: that is how every client writes them.
pub(crate) fn is_emoticon(attributes: &str) -> bool {
    attribute(attributes, "data-mx-emoticon").is_some()
        || ["alt", "title"].into_iter().any(|attr| {
            attribute(attributes, attr).is_some_and(|raw| {
                let name = decode_entities(raw);
                let name = name.trim();
                name.len() > 2
                    && name.starts_with(':')
                    && name.ends_with(':')
                    && !name[1..name.len() - 1].contains(|c: char| c == ':' || c.is_whitespace())
            })
        })
}

fn emoticon_label(attributes: &str) -> String {
    let name = ["alt", "title"].into_iter().find_map(|attr| {
        let raw = decode_entities(attribute(attributes, attr)?);
        let name = sanitize_line(&raw);
        let name: String = name
            .trim()
            .trim_matches(':')
            .trim()
            .chars()
            .take(64)
            .collect();
        (!is_blank(&name)).then(|| truncate(&name, 32))
    });
    match name {
        Some(name) => format!(":{name}:"),
        None => EMOTICON_PLACEHOLDER.to_owned(),
    }
}

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
    /// What this piece of a code block is, once [`crate::highlight`] looked at it.
    pub syntax: Option<crate::highlight::Syntax>,
    /// Index into [`Rich::emotes`]: this piece is a custom emoji, its text its name.
    pub emote: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub format: Format,
}

impl Segment {
    /// The custom emoji whose picture goes over this piece of a wrapped row, if it is
    /// one (see [`Emote::picture`]).
    pub fn emote_box(&self) -> Option<usize> {
        self.format.emote.filter(|_| self.text == EMOTE_BOX)
    }
}

/// A custom emoji in a message.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Emote {
    /// Its picture: a checked `mxc://` address.
    pub url: String,
    /// Whether [`wrap`] keeps two cells for its picture instead of showing its name.
    /// Decided by whoever lays the message out: the picture has to be there to draw.
    pub picture: bool,
}

/// Hand-written: the address stays out of logs.
impl std::fmt::Debug for Emote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Emote")
            .field("picture", &self.picture)
            .finish_non_exhaustive()
    }
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
    /// Index into [`Rich::code_blocks`] for the lines of a code block.
    pub block: Option<usize>,
}

/// A `<pre>` block's content as the sender wrote it, for copying.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodeBlock {
    /// The language from `class="language-…"`, if the sender gave one.
    pub lang: Option<String>,
    /// The raw content: original indentation and line breaks, without fence lines,
    /// and without the one newline that ends a block. Sanitised like all message text.
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rich {
    pub lines: Vec<RichLine>,
    pub links: Vec<String>,
    /// Every code block, in order, including empty ones (lines refer to them by index).
    pub code_blocks: Vec<CodeBlock>,
    /// Every custom emoji with a picture that could be shown, in order (pieces refer to
    /// them by index). One in a code block is only its name.
    pub emotes: Vec<Emote>,
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

/// Checks an RFC 5870 `geo:` URI so only safe, syntactically valid coordinates
/// with no whitespace, control characters, or shell-special markup become links.
pub fn safe_geo_uri(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("geo:") else {
        return false;
    };
    if rest.is_empty()
        || rest.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '\'' | '\\' | '`')
        })
    {
        return false;
    }
    let mut parts = rest.split(';');
    let Some(coords) = parts.next() else {
        return false;
    };
    let mut nums = coords.split(',');
    let (Some(lat_s), Some(lon_s)) = (nums.next(), nums.next()) else {
        return false;
    };
    let alt_s = nums.next();
    if nums.next().is_some() {
        return false;
    }
    let valid_coord_str = |v: &str| {
        !v.is_empty()
            && v.bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.'))
    };
    if !valid_coord_str(lat_s) || !valid_coord_str(lon_s) {
        return false;
    }
    let (Ok(lat), Ok(lon)) = (lat_s.parse::<f64>(), lon_s.parse::<f64>()) else {
        return false;
    };
    if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
        return false;
    }
    if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
        return false;
    }
    if let Some(alt_s) = alt_s {
        if !valid_coord_str(alt_s) {
            return false;
        }
        let Ok(alt) = alt_s.parse::<f64>() else {
            return false;
        };
        if !alt.is_finite() {
            return false;
        }
    }
    for param in parts {
        let Some((name, value)) = param.split_once('=') else {
            return false;
        };
        if name.is_empty()
            || value.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~' | b'%'))
        {
            return false;
        }
    }
    true
}

fn find_plain_link(rest: &str) -> Option<(usize, usize)> {
    let web = ["https://", "http://"]
        .into_iter()
        .filter_map(|scheme| rest.find(scheme).map(|pos| (pos, scheme.len())))
        .min_by_key(|&(pos, _)| pos);
    let is_boundary = |pos: usize| {
        pos == 0
            || !rest[..pos].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    };
    let matrix = rest
        .match_indices("matrix:")
        .find(|&(pos, _)| is_boundary(pos))
        .map(|(pos, s)| (pos, s.len()));
    let geo = rest
        .match_indices("geo:")
        .find(|&(pos, _)| {
            if !is_boundary(pos) {
                return false;
            }
            let tail = &rest[pos..];
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            let candidate =
                tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '\'', '"']);
            safe_geo_uri(candidate)
        })
        .map(|(pos, s)| (pos, s.len()));
    [web, matrix, geo]
        .into_iter()
        .flatten()
        .min_by_key(|&(pos, _)| pos)
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
pub(crate) fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
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
    /// The block being read while `pre` is set, as an index into `rich.code_blocks`.
    block: Option<usize>,
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
            block: self.block,
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
            // A void element: nothing to close, so no format to keep. Only custom emoji
            // are shown: by their name, and by their picture where it can be drawn.
            "img" => {
                if is_emoticon(attributes) {
                    let url = attribute(attributes, "src")
                        .map(|src| decode_entities(src).trim().to_owned())
                        .filter(|src| crate::matrix::is_valid_mxc(src));
                    if let Some(url) = url.filter(|_| !self.pre) {
                        format.emote = Some(self.rich.emotes.len());
                        self.rich.emotes.push(Emote {
                            url,
                            picture: false,
                        });
                    }
                    self.push_piece(&emoticon_label(attributes), &format);
                }
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
                self.block = Some(self.rich.code_blocks.len());
                self.rich.code_blocks.push(CodeBlock::default());
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block_break();
                format.bold = true;
            }
            "b" | "strong" => format.bold = true,
            "i" | "em" => format.italic = true,
            "u" | "ins" => format.underline = true,
            "s" | "del" | "strike" => format.strike = true,
            "code" | "tt" => {
                format.code = true;
                if let Some(block) = self.block
                    && let Some(lang) = attribute(attributes, "class").and_then(language)
                {
                    self.rich.code_blocks[block].lang.get_or_insert(lang);
                }
            }
            "a" => {
                if let Some(href) = attribute(attributes, "href") {
                    let href = sanitize(&decode_entities(href)).trim().to_owned();
                    // Only addresses that are safe to hand to the system opener.
                    let safe = (["https://", "http://", "mailto:", "matrix:"]
                        .iter()
                        .any(|scheme| href.starts_with(scheme))
                        && !href.contains(char::is_whitespace))
                        || safe_geo_uri(&href);
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
                // Whatever the sender's last newline was, it ends the block, it is
                // not a line of it.
                if let Some(block) = self.block.take() {
                    let text = &mut self.rich.code_blocks[block].text;
                    if text.ends_with('\n') {
                        text.pop();
                    }
                }
            }
            _ => {}
        }
    }
}

/// The language tag of a `class="language-rust"` attribute: short, and only characters
/// a language name has, so it is safe to show.
fn language(class: &str) -> Option<String> {
    let tag = class
        .split_whitespace()
        .find_map(|c| c.strip_prefix("language-"))?;
    let tag: String = tag.chars().take(24).collect();
    (!tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '+' | '-' | '#' | '.' | '_')))
    .then_some(tag)
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
        block: None,
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
                    let decoded = decode_entities(text);
                    // What is copied keeps its tabs (the display turns them into
                    // spaces); control characters and direction overrides are as
                    // dangerous in a paste as on screen, and go.
                    if let Some(block) = builder.block {
                        builder.rich.code_blocks[block].text.extend(
                            decoded
                                .chars()
                                .filter(|c| matches!(c, '\n' | '\t') || !unsafe_char(*c)),
                        );
                    }
                    builder.push_text(&sanitize(&decoded));
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
    /// The code block this row belongs to, if any.
    pub block: Option<usize>,
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
            block: line.block,
        };
        let mut used = 0;
        let next_row = |row: &mut WrappedRow, rows: &mut Vec<WrappedRow>, used: &mut usize| {
            let mut finished = std::mem::replace(
                row,
                WrappedRow {
                    prefix: line.hanging.clone(),
                    segments: Vec::new(),
                    block: line.block,
                },
            );
            // A space that fitted before the wrap point is not shown at the row's end
            // (the cells of an emoji's picture are not space).
            if !line.preformatted
                && let Some(last) = finished.segments.last_mut()
                && last.emote_box().is_none()
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
            // A custom emoji's picture: two cells that are never split. Where not even
            // they fit, the emoji keeps its name.
            let emote_width = width(EMOTE_BOX);
            if segment
                .format
                .emote
                .and_then(|e| rich.emotes.get(e))
                .is_some_and(|emote| emote.picture)
                && room >= emote_width
            {
                if used + emote_width > room && used > 0 {
                    next_row(&mut row, &mut rows, &mut used);
                }
                push(&mut row, EMOTE_BOX, &segment.format);
                used += emote_width;
                continue;
            }
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
    fn code_blocks_are_kept_raw_for_copying() {
        let rich = from_html(
            "<p>before</p><pre><code class=\"language-rust\">fn main() {\n    let x = 1;\n\n}\n</code></pre>\
             <p>between</p><pre>plain &lt;b&gt;\n\tkept</pre><pre><code class=\"language-x y\"></code></pre>",
        );
        assert_eq!(rich.code_blocks.len(), 3);
        // Indentation and the empty line stay; one trailing newline goes.
        assert_eq!(rich.code_blocks[0].text, "fn main() {\n    let x = 1;\n\n}");
        assert_eq!(rich.code_blocks[0].lang.as_deref(), Some("rust"));
        // A tab stays a tab in what is copied, though it is shown as spaces.
        assert_eq!(rich.code_blocks[1].text, "plain <b>\n\tkept");
        assert_eq!(rich.code_blocks[1].lang, None);
        assert!(rich.code_blocks[2].text.is_empty());
        // Every row of a block says which it is; the text around says nothing.
        let blocks: Vec<Option<usize>> = rich.lines.iter().map(|l| l.block).collect();
        assert_eq!(
            blocks,
            [
                None,
                Some(0),
                Some(0),
                Some(0),
                Some(0),
                None,
                Some(1),
                Some(1)
            ]
        );
        // Wrapping keeps it, also for rows cut at the edge.
        assert_eq!(
            wrap(&rich, 6)
                .iter()
                .filter(|row| row.block == Some(0))
                .count(),
            // "fn mai" "n() {" "    le" "t x = " "1;" "" "}"
            7
        );
    }

    #[test]
    fn code_language_tags_are_checked_and_trailing_newlines_kept_once() {
        for (class, expected) in [
            ("language-c++", Some("c++")),
            ("foo language-py bar", Some("py")),
            ("language-", None),
            ("language-a b", Some("a")),
            ("language-x\u{202e}y", None),
            ("hljs", None),
        ] {
            assert_eq!(language(class).as_deref(), expected, "{class}");
        }
        // Only the one newline that ends the block is not content.
        let rich = from_html("<pre><code>a\n\n</code></pre>");
        assert_eq!(rich.code_blocks[0].text, "a\n");
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

    #[test]
    fn geo_uris_are_validated_and_linked() {
        assert!(safe_geo_uri("geo:51.5074,-0.1278"));
        assert!(safe_geo_uri("geo:-33.8688,151.2093,12.5;crs=wgs84;u=35"));
        assert!(!safe_geo_uri("geo:95.0,0.0"));
        assert!(!safe_geo_uri("geo:0.0,185.0"));
        assert!(!safe_geo_uri("geo:51.5,-0.1;u=evil<script>"));
        assert!(!safe_geo_uri("geo:bad"));

        let plain = Rich::plain(
            "Meet at geo:51.5074,-0.1278;u=35. Skip geo:bad and still link https://example.org",
        );
        assert_eq!(
            plain.links,
            ["geo:51.5074,-0.1278;u=35", "https://example.org"]
        );

        let html =
            from_html("<a href=\"geo:48.8566,2.3522\">Paris</a> <a href=\"geo:999,0\">Nowhere</a>");
        assert_eq!(html.links, ["geo:48.8566,2.3522"]);
    }

    #[test]
    fn custom_emoji_show_as_their_shortcodes_and_other_pictures_as_nothing() {
        let rich = from_html(
            "hi <img data-mx-emoticon src=\"mxc://example.org/blob\" alt=\":blobcat:\" \
             title=\":blobcat:\" height=\"32\" /> and <b><img data-mx-emoticon \
             src=\"mxc://example.org/b\" title=\"party\"></b> <img src=\"mxc://example.org/pic\" \
             alt=\"a photo\">done",
        );
        assert_eq!(texts(&rich), ["hi :blobcat: and :party: done"]);
        assert!(
            rich.lines[0]
                .segments
                .iter()
                .any(|s| s.text == ":party:" && s.format.bold)
        );
        // The address itself is never shown.
        assert!(!texts(&rich)[0].contains("mxc"));
    }

    #[test]
    fn a_custom_emoji_is_known_by_its_name_when_its_mark_was_cleaned_away() {
        // What the SDK leaves of one: no `data-mx-emoticon`.
        let rich = from_html(
            "hello <img src=\"mxc://e.org/x\" alt=\":ratwave:\" title=\":ratwave:\" height=\"32\"> there",
        );
        let text: String = rich.lines[0]
            .segments
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(text, "hello :ratwave: there");
        // An ordinary picture in a message is still not shown.
        for html in [
            "a <img src=\"mxc://e.org/x\" alt=\"a photo\"> b",
            "a <img src=\"mxc://e.org/x\" alt=\"::\"> b",
            "a <img src=\"mxc://e.org/x\" alt=\":two words:\"> b",
            "a <img src=\"mxc://e.org/x\"> b",
        ] {
            let rich = from_html(html);
            let text: String = rich.lines[0]
                .segments
                .iter()
                .map(|s| s.text.as_str())
                .collect();
            assert_eq!(text, "a b", "{html}");
        }
    }

    #[test]
    fn custom_emoji_keep_their_address_for_the_picture_but_never_show_it() {
        let rich = from_html(
            "a <img data-mx-emoticon src=\"mxc://example.org/blob\" alt=\":blob:\"> b \
             <img src=\"https://example.org/x.png\" alt=\":web:\"> \
             <img src=\"mxc://example.org/a&quot;b\" alt=\":quote:\"> \
             <pre><img src=\"mxc://example.org/code\" alt=\":code:\"></pre>",
        );
        // Only a plain `mxc://` address outside code counts; the others are names.
        assert_eq!(rich.emotes.len(), 1);
        assert_eq!(rich.emotes[0].url, "mxc://example.org/blob");
        assert!(!rich.emotes[0].picture);
        assert_eq!(texts(&rich), ["a :blob: b :web: :quote:", ":code:"]);
        let emotes: Vec<(&str, Option<usize>)> = rich
            .lines
            .iter()
            .flat_map(|line| &line.segments)
            .filter(|s| s.format.emote.is_some())
            .map(|s| (s.text.as_str(), s.format.emote))
            .collect();
        assert_eq!(emotes, [(":blob:", Some(0))]);
        let debug = format!("{rich:?}");
        assert!(!debug.contains("mxc"), "{debug}");
    }

    #[test]
    fn a_custom_emoji_picture_takes_two_cells_and_its_name_until_then() {
        let html =
            "hi <img data-mx-emoticon src=\"mxc://example.org/blob\" alt=\":blobcat:\"> there";
        let mut rich = from_html(html);
        let shown = |rich: &Rich, max: usize| -> Vec<String> {
            wrap(rich, max)
                .iter()
                .map(|r| r.segments.iter().map(|s| s.text.as_str()).collect())
                .collect()
        };
        // Not there yet: its name, as before.
        assert_eq!(shown(&rich, 40), ["hi :blobcat: there"]);
        assert!(
            wrap(&rich, 40)[0]
                .segments
                .iter()
                .all(|s| s.emote_box().is_none())
        );

        rich.emotes[0].picture = true;
        let rows = wrap(&rich, 40);
        assert_eq!(shown(&rich, 40), [format!("hi {EMOTE_BOX} there")]);
        let boxes: Vec<usize> = rows[0]
            .segments
            .iter()
            .filter_map(Segment::emote_box)
            .collect();
        assert_eq!(boxes, [0]);
        // The two cells are never split, and are not taken for space at a row's end.
        assert_eq!(
            shown(&rich, 5),
            [format!("hi {EMOTE_BOX}"), "there".to_owned()]
        );
        let narrow = wrap(&rich, 4);
        assert_eq!(shown(&rich, 4)[0], "hi");
        assert_eq!(narrow[1].segments[0].emote_box(), Some(0));
        for max in 1..20 {
            for row in wrap(&rich, max) {
                let total: usize = row.segments.iter().map(|s| width(&s.text)).sum();
                assert!(total <= max, "{max}: {row:?}");
            }
        }
        // Where not even two cells fit, the name is shown, cut like any word.
        assert!(shown(&rich, 1).concat().contains("blobcat"));
    }

    #[test]
    fn a_hostile_custom_emoji_name_is_cleaned_and_kept_short() {
        let rich = from_html(&format!(
            "<img data-mx-emoticon src=\"mxc://e.org/x\" alt=\"\u{1b}[31m{}\nevil\">",
            "a".repeat(500)
        ));
        let text = texts(&rich).concat();
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\n'));
        assert!(
            text.starts_with(":[31maaa") && text.ends_with("…:"),
            "{text}"
        );
        assert!(width(&text) <= 34, "{text}");

        for html in [
            "<img data-mx-emoticon src=\"mxc://e.org/x\">",
            "<img data-mx-emoticon alt=\"::\" title=\"\u{200b}\">",
            "<img data-mx-emoticon alt=\"   \">",
        ] {
            assert_eq!(texts(&from_html(html)), [EMOTICON_PLACEHOLDER], "{html}");
        }
    }
}
