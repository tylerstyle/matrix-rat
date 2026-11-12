//! Turning what the user typed into a Matrix message: Markdown, plus slash commands.
//!
//! Matrix carries formatting as HTML next to a plain-text body; bridges translate that
//! HTML into each network's own formatting.

use comrak::Options;

/// What to do with a line of input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    /// A normal message. `html` is `None` when the text has no formatting.
    Text { plain: String, html: Option<String> },
    /// A `/me` action.
    Emote { plain: String, html: Option<String> },
    /// Nothing is sent; show this to the user (unknown command, missing argument).
    Rejected(String),
}

const SHRUG: &str = "¯\\_(ツ)_/¯";
const TABLEFLIP: &str = "(╯°□°)╯︵ ┻━┻";
const LENNY: &str = "( ͡° ͜ʖ ͡°)";

pub fn parse(input: &str) -> Outgoing {
    let input = input.trim();
    // `//text` sends a message that starts with a literal slash.
    if let Some(rest) = input.strip_prefix("//") {
        return markdown_message(&format!("/{rest}"));
    }
    let Some(command) = input.strip_prefix('/') else {
        return markdown_message(input);
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
    match name {
        "me" => needs_text(Outgoing::Emote {
            plain: argument.to_owned(),
            html: markdown_html(argument),
        }),
        "plain" => needs_text(Outgoing::Text {
            plain: argument.to_owned(),
            html: None,
        }),
        "html" => needs_text(Outgoing::Text {
            plain: strip_tags(argument),
            html: Some(argument.to_owned()),
        }),
        "spoiler" => needs_text(Outgoing::Text {
            plain: format!("||{argument}||"),
            html: Some(format!(
                "<span data-mx-spoiler>{}</span>",
                markdown_html(argument).unwrap_or_else(|| escape(argument))
            )),
        }),
        "rainbow" => needs_text(Outgoing::Text {
            plain: argument.to_owned(),
            html: Some(rainbow(argument)),
        }),
        "shrug" => markdown_message(&prepend(SHRUG, argument)),
        "tableflip" => markdown_message(&prepend(TABLEFLIP, argument)),
        "lenny" => markdown_message(&prepend(LENNY, argument)),
        _ => Outgoing::Rejected(format!(
            "Unknown command /{name}. Start with // to send it as text."
        )),
    }
}

fn prepend(face: &str, text: &str) -> String {
    if text.is_empty() {
        face.to_owned()
    } else {
        format!("{face} {text}")
    }
}

fn markdown_message(text: &str) -> Outgoing {
    Outgoing::Text {
        plain: text.to_owned(),
        html: markdown_html(text),
    }
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

    let html = comrak::markdown_to_html(text, &options);
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
    // Only line breaks and no other markup: the plain body says it all.
    let without_breaks = html.replace("<br />\n", "").replace("<br />", "");
    if without_breaks.contains('<') {
        Some(html)
    } else {
        None
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
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
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
        }
    }

    #[test]
    fn plain_text_has_no_html() {
        assert_eq!(parse("hello world"), text("hello world", None));
        assert_eq!(parse("  two\nlines  "), text("two\nlines", None));
        assert_eq!(parse("1 < 2 & 3 > 2"), text("1 < 2 & 3 > 2", None));
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
                html: Some("waves <em>hard</em>".into())
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
        assert!(matches!(parse("/nosuchthing x"), Outgoing::Rejected(_)));
        let Outgoing::Text { plain, html } = parse("/rainbow hi <") else {
            panic!();
        };
        assert_eq!(plain, "hi <");
        let html = html.unwrap();
        assert_eq!(html.matches("data-mx-color").count(), 3);
        assert!(html.contains("&lt;") && html.contains("#ff0000"));
    }
}
