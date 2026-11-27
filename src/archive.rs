//! A room's messages as a Markdown document: a header with the room's name and the
//! dates it covers, then the messages day by day, each with its time, its sender and
//! its text.
//!
//! Text only: a picture, a file or a voice message is a line that names it. What
//! people wrote is escaped, so that no message can start a heading, a list, a quote, a
//! link or HTML of its own in the document; a fenced code block in a message stays a
//! code block, behind a fence that nothing inside it can close.
//!
//! The document is written message by message to whatever takes it (a file, see
//! `crate::files::NewFile`); nothing of it is kept here.

use std::fmt;
use std::io::{self, Write};

use chrono::{DateTime, NaiveDate, TimeZone};

use crate::collect::{Cut, Entry, Kind, local};
use crate::text::sanitize_line;

/// The longest language word kept after a code fence.
const INFO_MAX: usize = 20;

/// The file name for the archive of a room called `room`, written on `day`.
pub fn file_name(room: &str, day: NaiveDate) -> String {
    let room: String = sanitize_line(room)
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let room: String = room
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let room: String = room.chars().take(60).collect();
    let room = if room.is_empty() {
        "room"
    } else {
        room.as_str()
    };
    format!("{room}-{}.md", day.format("%Y-%m-%d"))
}

/// One line of someone's text, made unable to be anything but text in Markdown.
/// Leading spaces become no-break spaces (they could make an indented code block).
pub fn escape(line: &str) -> String {
    let line = line.trim_end();
    let text = line.trim_start_matches(' ');
    let mut out = "\u{a0}".repeat(line.len() - text.len());
    let chars: Vec<char> = text.chars().collect();
    // A number at the start of a line, before `.` or `)`, starts a list.
    let digits = chars.iter().take_while(|c| c.is_ascii_digit()).count();
    for (at, &c) in chars.iter().enumerate() {
        let previous = at.checked_sub(1).map(|at| chars[at]);
        let next = chars.get(at + 1).copied();
        let needs = match c {
            '\\' | '`' | '*' | '[' | ']' | '<' | '~' => true,
            // Inside a word an underscore is only an underscore (addresses keep theirs).
            '_' => {
                !(previous.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric))
            }
            '&' => entity_follows(&chars[at + 1..]),
            // A bar anywhere: two lines with bars can make a table.
            '|' => true,
            '#' | '>' | '+' | '-' | '=' => at == 0,
            '.' | ')' => digits > 0 && at == digits,
            _ => false,
        };
        if needs {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether what follows an `&` would make it a character reference (`&amp;`, `&#60;`).
fn entity_follows(rest: &[char]) -> bool {
    let rest = match rest.first() {
        Some('#') => &rest[1..],
        _ => rest,
    };
    let name = rest
        .iter()
        .take_while(|c| c.is_ascii_alphanumeric())
        .count();
    (1..=32).contains(&name) && rest.get(name) == Some(&';')
}

/// A piece of a message's text.
#[derive(Debug, PartialEq, Eq)]
enum Piece<'a> {
    Line(&'a str),
    /// A fenced code block: the language word and the lines inside.
    Code(String, Vec<&'a str>),
}

/// If `line` opens or closes a code fence: how many backticks, and what follows them.
fn fence(line: &str) -> Option<(usize, &str)> {
    let line = line.trim_start();
    let ticks = line.chars().take_while(|c| *c == '`').count();
    (ticks >= 3).then(|| (ticks, line[ticks..].trim()))
}

/// The text of a message as lines and fenced code blocks. A fence that is never
/// closed holds everything after it, as Markdown reads it.
fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        match fence(line) {
            // With a backtick after the fence it is code inside a line, not a block.
            Some((ticks, info)) if !info.contains('`') => {
                let mut code = Vec::new();
                for inner in lines.by_ref() {
                    if fence(inner)
                        .is_some_and(|(closing, rest)| closing >= ticks && rest.is_empty())
                    {
                        break;
                    }
                    code.push(inner);
                }
                let info: String = info
                    .chars()
                    .take_while(|c| {
                        c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '.' | '_' | '-')
                    })
                    .take(INFO_MAX)
                    .collect();
                out.push(Piece::Code(info, code));
            }
            _ => out.push(Piece::Line(line)),
        }
    }
    out
}

/// Writes one archive.
pub struct Writer<W: Write, Tz: TimeZone> {
    out: W,
    zone: Tz,
    /// The day of the heading written last.
    day: Option<NaiveDate>,
    /// Whether any heading for a day was written (one for "no date" among them).
    headed: bool,
    written: usize,
}

impl<W: Write, Tz: TimeZone> Writer<W, Tz>
where
    Tz::Offset: fmt::Display,
{
    pub fn new(out: W, zone: Tz) -> Self {
        Self {
            out,
            zone,
            day: None,
            headed: false,
            written: 0,
        }
    }

    /// The top of the document: the room, when this was written, which days the
    /// messages are from (`first` and `last`, milliseconds since 1970), that it is
    /// text only, and if so that it is not complete.
    pub fn header(
        &mut self,
        room: &str,
        now: &DateTime<Tz>,
        first: Option<u64>,
        last: Option<u64>,
        cut: Option<Cut>,
    ) -> io::Result<()> {
        let room = escape(sanitize_line(room).trim());
        let room = if room.is_empty() {
            "Room".to_owned()
        } else {
            room
        };
        writeln!(self.out, "# {room}\n")?;
        writeln!(
            self.out,
            "Archive of a Matrix room, written by matrix-rat on {}.",
            now.format("%Y-%m-%d at %H:%M")
        )?;
        writeln!(self.out, "Times are local time (UTC{}).", now.format("%:z"))?;
        let day = |at: Option<u64>| {
            at.and_then(|at| local(&self.zone, at))
                .map(|at| at.format("%Y-%m-%d").to_string())
        };
        match (day(first), day(last)) {
            (Some(first), Some(last)) if first == last => {
                writeln!(self.out, "Messages of {first}.")?;
            }
            (Some(first), Some(last)) => writeln!(self.out, "Messages from {first} to {last}.")?,
            _ => writeln!(self.out, "No messages.")?,
        }
        writeln!(
            self.out,
            "\nText only: pictures, files, voice messages and other attachments are named, \
             not included."
        )?;
        match cut {
            Some(Cut::Limit) => writeln!(
                self.out,
                "\nNot complete: the room has more messages than one archive takes; these \
                 are the newest."
            )?,
            Some(Cut::LoadFailed) => writeln!(
                self.out,
                "\nNot complete: older messages could not be loaded when this was written."
            )?,
            None => {}
        }
        writeln!(self.out)
    }

    /// The next message, under a heading for its day where a new day begins.
    pub fn entry(&mut self, entry: &Entry) -> io::Result<()> {
        let when = entry.at.and_then(|at| local(&self.zone, at));
        let day = when.as_ref().map(DateTime::date_naive);
        // A message without a time stays under the day before it.
        if !self.headed || (day.is_some() && day != self.day) {
            match &when {
                Some(when) => writeln!(self.out, "## {}\n", when.format("%Y-%m-%d, %A"))?,
                None => writeln!(self.out, "## Without a date\n")?,
            }
            self.headed = true;
            self.day = day;
        }
        let time = when
            .map(|when| when.format("%H:%M").to_string())
            .unwrap_or_else(|| "--:--".to_owned());
        for (at, block) in blocks(&time, entry).iter().enumerate() {
            if at > 0 {
                writeln!(self.out)?;
            }
            self.out.write_all(block.as_bytes())?;
        }
        writeln!(self.out)?;
        self.written += 1;
        Ok(())
    }

    /// How many messages were written, and what they were written to.
    pub fn finish(self) -> (W, usize) {
        (self.out, self.written)
    }
}

/// A message as Markdown blocks (paragraphs and code blocks), each ending in a line
/// break; a blank line goes between them.
fn blocks(time: &str, entry: &Entry) -> Vec<String> {
    let sender = escape(&entry.sender);
    let sender = if sender.is_empty() {
        "Unknown".to_owned()
    } else {
        sender
    };
    let name = escape(entry.name.as_deref().unwrap_or_default());
    let mut text = entry.text.as_str();
    let mut head = match &entry.kind {
        // The action or the event reads on in the same line.
        Kind::Emote => {
            let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
            text = rest;
            format!("**{time}** \\* {sender} {}", escape(first))
        }
        Kind::Event => {
            let line = escape(&sanitize_line(text));
            text = "";
            format!("**{time}** *{line}*")
        }
        Kind::Text => format!("**{time} {sender}**"),
        Kind::Poll => format!("**{time} {sender}** (poll)"),
        Kind::Deleted => format!("**{time} {sender}** *(message deleted)*"),
        Kind::Undecryptable => format!("**{time} {sender}** *(could not be decrypted)*"),
        Kind::Media(what) => format!("**{time} {sender}** \\[{what}: {name}\\]"),
    };
    if entry.edited {
        head.push_str(" (edited)");
    }
    let mut out = Vec::new();
    // The lines of the paragraph that is being put together.
    let mut paragraph = vec![head];
    if let Some(reply) = &entry.reply {
        paragraph.push(format!("↪ {}", escape(reply)));
    }
    let close = |paragraph: &mut Vec<String>, out: &mut Vec<String>| {
        if !paragraph.is_empty() {
            // A backslash before the line break keeps the lines apart when shown.
            out.push(format!("{}\n", paragraph.join("\\\n")));
            paragraph.clear();
        }
    };
    for piece in pieces(text) {
        match piece {
            Piece::Line(line) if line.trim().is_empty() => close(&mut paragraph, &mut out),
            Piece::Line(line) => paragraph.push(escape(line)),
            Piece::Code(info, code) => {
                close(&mut paragraph, &mut out);
                // Longer than any row of backticks a line inside begins with.
                let longest = code
                    .iter()
                    .map(|line| line.trim_start().chars().take_while(|c| *c == '`').count())
                    .max()
                    .unwrap_or(0);
                let ticks = "`".repeat(longest.max(2) + 1);
                let mut block = format!("{ticks}{info}\n");
                for line in code {
                    block.push_str(line);
                    block.push('\n');
                }
                block.push_str(&ticks);
                block.push('\n');
                out.push(block);
            }
        }
    }
    close(&mut paragraph, &mut out);
    if !entry.reactions.is_empty() {
        let reactions: Vec<String> = entry
            .reactions
            .iter()
            .map(|(key, count)| format!("{} {count}", escape(key)))
            .collect();
        out.push(format!("Reactions: {}\n", reactions.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collect::entry;
    use crate::collect::tests::{AT, said};
    use chrono::Utc;

    const DAY: u64 = 86_400_000;

    fn written(entries: &[Entry]) -> String {
        let mut writer = Writer::new(Vec::new(), Utc);
        for entry in entries {
            writer.entry(entry).unwrap();
        }
        String::from_utf8(writer.finish().0).unwrap()
    }

    fn one(body: &str) -> String {
        written(&[entry(&said("Ann", body)).unwrap()])
    }

    #[test]
    fn what_people_write_cannot_become_structure() {
        for (typed, escaped) in [
            ("# not a heading", "\\# not a heading"),
            ("> not a quote", "\\> not a quote"),
            ("- not a list", "\\- not a list"),
            ("+ not a list", "\\+ not a list"),
            ("* not a list", "\\* not a list"),
            ("1. not a list", "1\\. not a list"),
            ("12) not a list", "12\\) not a list"),
            ("---", "\\---"),
            ("===", "\\==="),
            ("___", "\\_\\_\\_"),
            ("| a | b |", "\\| a \\| b \\|"),
            ("a | b", "a \\| b"),
            ("--- | ---", "\\--- \\| ---"),
            ("    not code", "\u{a0}\u{a0}\u{a0}\u{a0}not code"),
            ("<script>alert(1)</script>", "\\<script>alert(1)\\</script>"),
            (
                "[click](http://evil.example)",
                "\\[click\\](http://evil.example)",
            ),
            (
                "**bold** and _it_ and `code`",
                "\\*\\*bold\\*\\* and \\_it\\_ and \\`code\\`",
            ),
            ("a\\b", "a\\\\b"),
            ("~~~", "\\~\\~\\~"),
            ("&lt;b&gt; &#60; AT&T", "\\&lt;b\\&gt; \\&#60; AT&T"),
            // Left as they are: an underscore inside a word, a number inside a line.
            ("snake_case_name at 3. place", "snake_case_name at 3. place"),
            (
                "https://example.org/a_b?x=1&y=2",
                "https://example.org/a_b?x=1&y=2",
            ),
            ("ends with a backslash\\", "ends with a backslash\\\\"),
            ("trailing spaces   ", "trailing spaces"),
        ] {
            assert_eq!(escape(typed), escaped, "{typed}");
        }
    }

    #[test]
    fn a_message_is_its_time_its_sender_and_its_lines() {
        assert_eq!(
            one("first line\nsecond line\n\n# third"),
            "## 2026-11-20, Friday\n\n\
             **14:03 Ann**\\\nfirst line\\\nsecond line\n\n\\# third\n\n"
        );
    }

    #[test]
    fn the_name_of_a_sender_cannot_break_out_either() {
        let text = written(&[entry(&said("**Ann**\n# Boss", "hi")).unwrap()]);
        assert!(
            text.contains("**14:03 \\*\\*Ann\\*\\* # Boss**\\\nhi\n"),
            "{text}"
        );
    }

    #[test]
    fn a_code_block_stays_a_code_block_that_nothing_inside_can_close() {
        let text = one("look:\n```rust\nfn main() {}\n# not escaped *here*\n```\nafter");
        assert_eq!(
            text,
            "## 2026-11-20, Friday\n\n\
             **14:03 Ann**\\\nlook:\n\n\
             ```rust\nfn main() {}\n# not escaped *here*\n```\n\n\
             after\n\n"
        );
        // Backticks inside: the fence around them is longer.
        let text = one("````\n```\ninner\n```\n````");
        assert!(text.contains("\n````\n```\ninner\n```\n````\n"), "{text}");
        // Never closed: everything after it is code, and the document goes on after.
        let text = written(&[
            entry(&said("Ann", "```\nopen\n# still code")).unwrap(),
            entry(&said("Bob", "# next")).unwrap(),
        ]);
        assert!(
            text.contains("```\nopen\n# still code\n```\n\n**14:03 Bob**\\\n\\# next\n"),
            "{text}"
        );
        // What follows the fence is one harmless word.
        let text = one("```sh <b>&\"x\nls\n```");
        assert!(text.contains("\n```sh\nls\n```\n"), "{text}");
        // Code inside a line is text with its backticks.
        assert!(one("```not a block```").contains("\\`\\`\\`not a block\\`\\`\\`"));
    }

    #[test]
    fn messages_are_grouped_by_day() {
        let mut first = said("Ann", "one");
        first.sent_at = Some(AT - DAY);
        let mut undated = said("Cy", "three");
        undated.sent_at = None;
        let text = written(&[
            entry(&first).unwrap(),
            entry(&said("Bob", "two")).unwrap(),
            entry(&undated).unwrap(),
            entry(&said("Ann", "four")).unwrap(),
        ]);
        assert_eq!(
            text,
            "## 2026-11-19, Thursday\n\n**14:03 Ann**\\\none\n\n\
             ## 2026-11-20, Friday\n\n**14:03 Bob**\\\ntwo\n\n\
             **--:-- Cy**\\\nthree\n\n\
             **14:03 Ann**\\\nfour\n\n"
        );
        // Nothing dated before it: a heading of its own.
        assert!(written(&[entry(&undated).unwrap()]).starts_with("## Without a date\n\n"));
    }

    #[test]
    fn replies_edits_reactions_and_the_other_kinds_show_as_such() {
        use crate::model::{MsgKind, Reaction};
        let mut reply = said("Ann", "yes");
        reply.reply_to = Some(("Bob".to_owned(), "# shall we?".to_owned()));
        reply.edited = true;
        reply.reactions = vec![
            Reaction {
                key: "👍".to_owned(),
                count: 2,
                own: true,
                failed: false,
            },
            Reaction {
                key: "*".to_owned(),
                count: 1,
                own: false,
                failed: false,
            },
        ];
        assert_eq!(
            written(&[entry(&reply).unwrap()]),
            "## 2026-11-20, Friday\n\n\
             **14:03 Ann** (edited)\\\n↪ Bob: # shall we?\\\nyes\n\n\
             Reactions: 👍 2, \\* 1\n\n"
        );

        let mut message = said("Ann", "waves\nand smiles");
        message.kind = MsgKind::Emote;
        assert!(
            written(&[entry(&message).unwrap()])
                .contains("**14:03** \\* Ann waves\\\nand smiles\n")
        );
        message.kind = MsgKind::System;
        message.body = "Ann changed the topic".to_owned();
        assert!(
            written(&[entry(&message).unwrap()]).contains("**14:03** *Ann changed the topic*\n")
        );
        message.kind = MsgKind::Redacted;
        assert!(
            written(&[entry(&message).unwrap()]).contains("**14:03 Ann** *(message deleted)*\n")
        );
        message.kind = MsgKind::Undecryptable;
        assert!(
            written(&[entry(&message).unwrap()])
                .contains("**14:03 Ann** *(could not be decrypted)*\n")
        );
    }

    #[test]
    fn media_is_a_line_that_names_it() {
        let media = Entry {
            at: Some(AT),
            sender: "Ann".to_owned(),
            kind: Kind::Media("Picture"),
            text: "our cat".to_owned(),
            name: Some("cat](http://evil.example).png".to_owned()),
            reply: None,
            edited: false,
            reactions: Vec::new(),
        };
        assert_eq!(
            written(&[media]),
            "## 2026-11-20, Friday\n\n\
             **14:03 Ann** \\[Picture: cat\\](http://evil.example).png\\]\\\nour cat\n\n"
        );
    }

    #[test]
    fn the_header_names_the_room_the_days_and_that_it_is_text_only() {
        let now = local(&Utc, AT).unwrap();
        let mut writer = Writer::new(Vec::new(), Utc);
        writer
            .header("# Plans <b>", &now, Some(AT - 3 * DAY), Some(AT), None)
            .unwrap();
        let text = String::from_utf8(writer.finish().0).unwrap();
        assert_eq!(
            text,
            "# \\# Plans \\<b>\n\n\
             Archive of a Matrix room, written by matrix-rat on 2026-11-20 at 14:03.\n\
             Times are local time (UTC+00:00).\n\
             Messages from 2026-11-17 to 2026-11-20.\n\n\
             Text only: pictures, files, voice messages and other attachments are named, \
             not included.\n\n"
        );

        let mut writer = Writer::new(Vec::new(), Utc);
        writer
            .header("", &now, Some(AT), Some(AT), Some(Cut::Limit))
            .unwrap();
        let text = String::from_utf8(writer.finish().0).unwrap();
        assert!(text.starts_with("# Room\n\n"), "{text}");
        assert!(text.contains("Messages of 2026-11-20.\n"), "{text}");
        assert!(
            text.contains("Not complete: the room has more messages"),
            "{text}"
        );

        let mut writer = Writer::new(Vec::new(), Utc);
        writer
            .header("Plans", &now, None, None, Some(Cut::LoadFailed))
            .unwrap();
        let text = String::from_utf8(writer.finish().0).unwrap();
        assert!(text.contains("No messages.\n"), "{text}");
        assert!(
            text.contains("older messages could not be loaded"),
            "{text}"
        );
    }

    #[test]
    fn the_count_is_of_the_messages_written() {
        let mut writer = Writer::new(Vec::new(), Utc);
        for n in 0..3 {
            writer
                .entry(&entry(&said("Ann", &format!("{n}"))).unwrap())
                .unwrap();
        }
        assert_eq!(writer.finish().1, 3);
    }

    #[test]
    fn the_file_is_named_after_the_room_and_the_day() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        assert_eq!(file_name("Plans & Ideas", day), "Plans-Ideas-2026-11-20.md");
        assert_eq!(
            file_name("../../etc/.passwd", day),
            "etc-passwd-2026-11-20.md"
        );
        assert_eq!(file_name("", day), "room-2026-11-20.md");
        assert_eq!(file_name("🙂", day), "room-2026-11-20.md");
        assert_eq!(file_name("Küche", day), "Küche-2026-11-20.md");
        assert!(file_name(&"x".repeat(500), day).len() < 80);
    }
}
