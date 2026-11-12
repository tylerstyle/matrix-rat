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
        "html" if strip_tags(argument).trim().is_empty() => {
            Outgoing::Rejected("/html needs some text between the tags".to_owned())
        }
        "html" => Outgoing::Text {
            plain: strip_tags(argument).trim().to_owned(),
            html: Some(argument.to_owned()),
        },
        "spoiler" => needs_text(Outgoing::Text {
            plain: format!("||{argument}||"),
            html: Some(format!(
                "<span data-mx-spoiler>{}</span>",
                inline_html(argument)
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
        let Outgoing::Text { plain, html } = parse("/rainbow hi <") else {
            panic!();
        };
        assert_eq!(plain, "hi <");
        let html = html.unwrap();
        assert_eq!(html.matches("data-mx-color").count(), 3);
        assert!(html.contains("&lt;") && html.contains("#ff0000"));
    }
}
