//! Colours for code blocks: marks the pieces of a block whose sender named its language
//! as keywords, strings, comments and so on. The theme turns those marks into colours.
//!
//! The code comes from other people, so the work is bounded: blocks over a size are left
//! plain, a block that takes too long is left plain, and a grammar that gives up on a
//! line (its matcher has a step limit) leaves the block plain. Outcomes are remembered,
//! so a block is looked at once however often the timeline is laid out again.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxSet};

use crate::rich::{Rich, Segment};

/// What a piece of code is, as far as colours go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Syntax {
    /// Code without a mark of its own inside a coloured block.
    Plain,
    Keyword,
    String,
    Comment,
    /// Numbers and named constants.
    Constant,
    Type,
    Function,
}

/// Blocks with more lines stay plain.
const MAX_LINES: usize = 600;
/// Blocks with more text stay plain.
const MAX_BYTES: usize = 48 * 1024;
/// A line longer than this (minified code) makes the block stay plain.
const MAX_LINE_BYTES: usize = 2000;
/// One block may take this long, then it stays plain. Generous, because the first block
/// in a language also prepares that language's grammar.
const BUDGET: Duration = Duration::from_millis(200);
/// After this many blocks ran out of time, new blocks stay plain until the program ends:
/// somebody is sending code that is made to be slow.
const MAX_OVERRUNS: usize = 3;
/// Remembered outcomes; when full, all are forgotten.
const REMEMBERED: usize = 256;

/// One line's pieces: byte length and mark, in order, covering the line.
type Marks = Vec<Vec<(usize, Syntax)>>;

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);

/// Scope prefixes and what they mean; the first that matches wins, so the more specific
/// come first.
static KINDS: LazyLock<Vec<(Scope, Syntax)>> = LazyLock::new(|| {
    [
        ("comment", Syntax::Comment),
        ("string", Syntax::String),
        ("constant.character.escape", Syntax::Constant),
        ("constant", Syntax::Constant),
        ("keyword", Syntax::Keyword),
        ("storage", Syntax::Keyword),
        ("entity.name.tag", Syntax::Keyword),
        ("entity.name.function", Syntax::Function),
        ("support.function", Syntax::Function),
        ("variable.function", Syntax::Function),
        ("entity.name", Syntax::Type),
        ("entity.other.inherited-class", Syntax::Type),
        ("entity.other.attribute-name", Syntax::Type),
        ("support.type", Syntax::Type),
        ("support.class", Syntax::Type),
    ]
    .into_iter()
    .filter_map(|(name, kind)| Some((Scope::new(name).ok()?, kind)))
    .collect()
});

static OVERRUNS: AtomicUsize = AtomicUsize::new(0);

static MEMORY: LazyLock<Mutex<HashMap<u64, Option<Arc<Marks>>>>> = LazyLock::new(Mutex::default);

/// Marks the code blocks of `rich` that have a known language.
pub fn colour(rich: &mut Rich) {
    for block in 0..rich.code_blocks.len() {
        let Some(lang) = rich.code_blocks[block].lang.clone() else {
            continue;
        };
        let lines: Vec<usize> = (0..rich.lines.len())
            .filter(|&i| rich.lines[i].block == Some(block))
            .collect();
        let texts: Vec<String> = lines
            .iter()
            .map(|&i| {
                rich.lines[i]
                    .segments
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect()
            })
            .collect();
        let Some(marks) = remembered(&lang, &texts) else {
            continue;
        };
        for (&line, marks) in lines.iter().zip(marks.iter()) {
            let segments = std::mem::take(&mut rich.lines[line].segments);
            rich.lines[line].segments = apply(segments, marks);
        }
    }
}

fn remembered(lang: &str, lines: &[String]) -> Option<Arc<Marks>> {
    let mut hasher = DefaultHasher::new();
    lang.hash(&mut hasher);
    lines.hash(&mut hasher);
    let key = hasher.finish();
    let mut memory = MEMORY.lock().ok()?;
    if let Some(known) = memory.get(&key) {
        return known.clone();
    }
    let marks = marks(lang, lines).map(Arc::new);
    if memory.len() >= REMEMBERED {
        memory.clear();
    }
    memory.insert(key, marks.clone());
    marks
}

/// The marks for every line, or `None` when the block stays plain.
fn marks(lang: &str, lines: &[String]) -> Option<Marks> {
    if lines.len() > MAX_LINES
        || lines.iter().map(String::len).sum::<usize>() > MAX_BYTES
        || lines.iter().any(|l| l.len() > MAX_LINE_BYTES)
    {
        return None;
    }
    if OVERRUNS.load(Ordering::Relaxed) >= MAX_OVERRUNS {
        return None;
    }
    let syntaxes = &*SYNTAXES;
    let syntax = syntaxes.find_syntax_by_token(lang)?;
    // "Plain Text" has nothing to mark.
    if syntax.name == syntaxes.find_syntax_plain_text().name {
        return None;
    }
    let started = Instant::now();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut all = Vec::with_capacity(lines.len());
    let mut marked = false;
    for line in lines {
        if started.elapsed() > BUDGET {
            OVERRUNS.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let with_end = format!("{line}\n");
        let ops = state.parse_line(&with_end, syntaxes).ok()?;
        let mut pieces: Vec<(usize, Syntax)> = Vec::new();
        let mut at = 0;
        let mut push = |from: usize, to: usize, stack: &ScopeStack| {
            // The grammar sees the line end; the line itself does not have it.
            let to = to.min(line.len());
            if to <= from || !line.is_char_boundary(from) || !line.is_char_boundary(to) {
                return;
            }
            let kind = kind(stack);
            match pieces.last_mut() {
                Some((len, last)) if *last == kind => *len += to - from,
                _ => pieces.push((to - from, kind)),
            }
        };
        for (offset, op) in ops {
            push(at, offset, &stack);
            at = at.max(offset.min(line.len()));
            stack.apply(&op).ok()?;
        }
        push(at, line.len(), &stack);
        if pieces.iter().map(|(len, _)| len).sum::<usize>() != line.len() {
            // A piece fell on something that is not a character boundary: no colours.
            return None;
        }
        marked |= pieces.iter().any(|(_, kind)| *kind != Syntax::Plain);
        all.push(pieces);
    }
    marked.then_some(all)
}

/// The innermost scope that means something decides.
fn kind(stack: &ScopeStack) -> Syntax {
    stack
        .as_slice()
        .iter()
        .rev()
        .find_map(|scope| {
            KINDS
                .iter()
                .find(|(prefix, _)| prefix.is_prefix_of(*scope))
                .map(|(_, kind)| *kind)
        })
        .unwrap_or(Syntax::Plain)
}

/// Cuts `segments` where the marks change, keeping each segment's own format.
fn apply(segments: Vec<Segment>, marks: &[(usize, Syntax)]) -> Vec<Segment> {
    let mut out = Vec::with_capacity(segments.len().max(marks.len()));
    let mut marks = marks.iter().copied();
    let mut current = marks.next();
    for segment in segments {
        let mut rest = segment.text.as_str();
        while !rest.is_empty() {
            let Some((left, kind)) = current.as_mut() else {
                // The marks ran out (they should not): the rest stays as it was.
                out.push(Segment {
                    text: rest.to_owned(),
                    format: segment.format.clone(),
                });
                break;
            };
            let take = (*left).min(rest.len());
            if take == 0 || !rest.is_char_boundary(take) {
                out.push(Segment {
                    text: rest.to_owned(),
                    format: segment.format.clone(),
                });
                current = None;
                break;
            }
            let mut format = segment.format.clone();
            format.syntax = Some(*kind);
            out.push(Segment {
                text: rest[..take].to_owned(),
                format,
            });
            rest = &rest[take..];
            *left -= take;
            if *left == 0 {
                current = marks.next();
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rich::from_html;

    fn coloured(html: &str) -> Rich {
        let mut rich = from_html(html);
        colour(&mut rich);
        rich
    }

    /// The pieces of one line as (text, mark).
    fn pieces(rich: &Rich, line: usize) -> Vec<(&str, Option<Syntax>)> {
        rich.lines[line]
            .segments
            .iter()
            .map(|s| (s.text.as_str(), s.format.syntax))
            .collect()
    }

    fn mark_of(rich: &Rich, text: &str) -> Option<Syntax> {
        rich.lines
            .iter()
            .flat_map(|l| &l.segments)
            .find(|s| s.text.trim() == text)
            .and_then(|s| s.format.syntax)
    }

    #[test]
    fn a_block_with_a_known_language_is_marked() {
        let rich = coloured(
            "<pre><code class=\"language-rust\">// hello\nfn main() {\n    let x = \"a\";\n    let n = 42;\n}\n</code></pre>",
        );
        assert_eq!(mark_of(&rich, "// hello"), Some(Syntax::Comment));
        assert_eq!(mark_of(&rich, "fn"), Some(Syntax::Keyword));
        assert_eq!(mark_of(&rich, "main"), Some(Syntax::Function));
        assert_eq!(mark_of(&rich, "let"), Some(Syntax::Keyword));
        assert_eq!(mark_of(&rich, "\"a\""), Some(Syntax::String));
        assert_eq!(mark_of(&rich, "42"), Some(Syntax::Constant));
    }

    #[test]
    fn the_text_is_never_changed() {
        let html = "<pre><code class=\"language-py\">def f(a):\n\treturn \"ü→\" + a  # ok\n\n</code></pre>";
        let plain = from_html(html);
        let rich = coloured(html);
        assert_eq!(plain.lines.len(), rich.lines.len());
        for (a, b) in plain.lines.iter().zip(&rich.lines) {
            let text = |l: &crate::rich::RichLine| -> String {
                l.segments.iter().map(|s| s.text.as_str()).collect()
            };
            assert_eq!(text(a), text(b));
            assert_eq!(a.block, b.block);
            assert_eq!(a.preformatted, b.preformatted);
        }
        assert_eq!(plain.code_blocks, rich.code_blocks);
        assert_eq!(mark_of(&rich, "def"), Some(Syntax::Keyword));
    }

    #[test]
    fn blocks_without_a_language_or_with_an_unknown_one_stay_as_they_are() {
        for html in [
            "<pre><code>fn main() {}\n</code></pre>",
            "<pre><code class=\"language-nosuchlanguage\">fn main() {}\n</code></pre>",
            "<pre><code class=\"language-txt\">fn main() {}\n</code></pre>",
        ] {
            assert_eq!(coloured(html), from_html(html), "{html}");
        }
    }

    #[test]
    fn text_around_a_block_is_left_alone() {
        let rich = coloured(
            "<p>let before</p><pre><code class=\"language-rust\">let x = 1;\n</code></pre><p>let after</p>",
        );
        for line in rich.lines.iter().filter(|l| l.block.is_none()) {
            assert!(line.segments.iter().all(|s| s.format.syntax.is_none()));
        }
        assert_eq!(mark_of(&rich, "let"), Some(Syntax::Keyword));
    }

    #[test]
    fn a_string_over_several_lines_keeps_its_mark() {
        let rich = coloured(
            "<pre><code class=\"language-python\">x = \"\"\"one\ntwo\nthree\"\"\"\ny = 1\n</code></pre>",
        );
        assert_eq!(pieces(&rich, 1), vec![("two", Some(Syntax::String))]);
    }

    #[test]
    fn two_blocks_are_marked_each_in_its_language() {
        let rich = coloured(
            "<pre><code class=\"language-json\">{\"a\": true}\n</code></pre>\
             <pre><code class=\"language-sh\">echo hi # c\n</code></pre>",
        );
        assert_eq!(mark_of(&rich, "true"), Some(Syntax::Constant));
        assert_eq!(mark_of(&rich, "# c"), Some(Syntax::Comment));
    }

    #[test]
    fn big_blocks_stay_plain() {
        let many = "let x = 1;\n".repeat(MAX_LINES + 1);
        let long = format!("let x = \"{}\";\n", "a".repeat(MAX_LINE_BYTES));
        for code in [many, long] {
            let html = format!("<pre><code class=\"language-rust\">{code}</code></pre>");
            assert_eq!(coloured(&html), from_html(&html));
        }
    }

    #[test]
    fn marks_that_do_not_fit_leave_the_rest_as_it_was() {
        let segment = |text: &str| Segment {
            text: text.to_owned(),
            format: Default::default(),
        };
        // Too short, and one that would cut a character in two.
        let out = apply(vec![segment("abcdef")], &[(2, Syntax::Keyword)]);
        assert_eq!(
            out.iter().map(|s| s.text.as_str()).collect::<String>(),
            "abcdef"
        );
        assert_eq!(out[1].format.syntax, None);
        let out = apply(
            vec![segment("aü")],
            &[(2, Syntax::Keyword), (1, Syntax::Plain)],
        );
        assert_eq!(
            out.iter().map(|s| s.text.as_str()).collect::<String>(),
            "aü"
        );
    }

    #[test]
    fn a_format_inside_a_block_survives() {
        let mut bold = Segment {
            text: "let".to_owned(),
            format: Default::default(),
        };
        bold.format.bold = true;
        let out = apply(
            vec![
                bold,
                Segment {
                    text: " x".to_owned(),
                    format: Default::default(),
                },
            ],
            &[(3, Syntax::Keyword), (2, Syntax::Plain)],
        );
        assert!(out[0].format.bold);
        assert_eq!(out[0].format.syntax, Some(Syntax::Keyword));
        assert!(!out[1].format.bold);
    }
}
