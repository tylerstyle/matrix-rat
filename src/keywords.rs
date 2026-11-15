//! What the UI knows about the account's notification keywords: the popup, and the
//! checks a new keyword has to pass. The keywords are push rules on the server, shared
//! with the account's other clients; this program stores none of them.

use crate::lineedit::LineEdit;
use crate::matrix::{MAX_KEYWORDS, shown_keywords};
use crate::text::{is_blank, sanitize_line, truncate, unsafe_char};

/// The most characters a keyword may have.
pub const MAX_CHARS: usize = 64;

/// What the demo starts with: both are in its messages.
const DEMO: [&str; 2] = ["lunch", "sliding sync"];

/// What the app keeps for the popup.
pub struct KeywordsState {
    /// The number of the watch the worker runs for the open popup. A list that quotes
    /// another number is for a popup that was closed (and maybe opened again), and
    /// dropped.
    pub watch: Option<u64>,
    pub next_watch: u64,
    /// Numbers the requests; answers quote theirs back.
    pub next_request: u64,
    /// The demo has no server: its keywords live here while it runs. Unused otherwise.
    pub demo: Vec<String>,
}

impl Default for KeywordsState {
    fn default() -> Self {
        Self {
            watch: None,
            next_watch: 0,
            next_request: 0,
            demo: DEMO.map(str::to_owned).to_vec(),
        }
    }
}

/// Keywords say what the user cares about: counts only.
impl std::fmt::Debug for KeywordsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeywordsState")
            .field("watch", &self.watch)
            .field("next_request", &self.next_request)
            .field("demo", &self.demo.len())
            .finish_non_exhaustive()
    }
}

/// A change the popup asked the worker for and waits to hear about.
#[derive(Clone, PartialEq, Eq)]
pub enum Pending {
    Add(String),
    Remove(String),
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Add(_) => "Add",
            Self::Remove(_) => "Remove",
        })
    }
}

#[derive(Clone)]
pub struct KeywordsPopup {
    /// `None` until the worker has sent the list.
    pub list: Option<Vec<String>>,
    /// How many keywords the account has: more than the list holds when it was cut.
    pub total: usize,
    pub input: LineEdit,
    pub selected: usize,
    pub scroll: usize,
    /// List rows as of the last draw; PageUp/PageDown move by it.
    pub page: usize,
    pub reveal: bool,
    /// The keyword whose removal was asked about: removing it once more does it.
    pub confirm: Option<String>,
    /// The request the worker is busy with, and what it is about.
    pub pending: Option<(u64, Pending)>,
    pub error: Option<String>,
}

/// Neither the keywords nor what is being typed.
impl std::fmt::Debug for KeywordsPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeywordsPopup")
            .field("listed", &self.list.as_ref().map(Vec::len))
            .field("total", &self.total)
            .field("selected", &self.selected)
            .field("confirm", &self.confirm.is_some())
            .field("pending", &self.pending)
            .field("error", &self.error.is_some())
            .finish_non_exhaustive()
    }
}

impl KeywordsPopup {
    /// Bytes the field takes: the most characters a keyword may have, at four bytes
    /// each, and some spaces around them (which are cut).
    pub const INPUT_MAX_BYTES: usize = MAX_CHARS * 4 + 16;

    /// With the list, or waiting for it.
    pub fn new(list: Option<(Vec<String>, usize)>) -> Self {
        let (list, total) = match list {
            Some((keywords, total)) => (Some(keywords), total),
            None => (None, 0),
        };
        Self {
            list,
            total,
            input: LineEdit::new().with_max_bytes(Self::INPUT_MAX_BYTES),
            selected: 0,
            scroll: 0,
            page: 0,
            reveal: true,
            confirm: None,
            pending: None,
            error: None,
        }
    }

    pub fn keywords(&self) -> &[String] {
        self.list.as_deref().unwrap_or(&[])
    }

    pub fn selected_keyword(&self) -> Option<&str> {
        self.keywords().get(self.selected).map(String::as_str)
    }

    /// A new list: the selection stays on its keyword, and a question about a
    /// keyword that is gone is dropped.
    pub fn set_list(&mut self, keywords: Vec<String>, total: usize) {
        let keep = self.selected_keyword().map(str::to_owned);
        self.selected = keep
            .and_then(|wanted| keywords.iter().position(|keyword| *keyword == wanted))
            .unwrap_or_else(|| self.selected.min(keywords.len().saturating_sub(1)));
        if let Some(asked) = &self.confirm
            && !keywords.contains(asked)
        {
            self.confirm = None;
        }
        self.list = Some(keywords);
        self.total = total;
        self.reveal = true;
    }

    /// A keyword was added: it shows at once, selected (the worker's list follows).
    pub fn added(&mut self, keyword: &str) {
        let mut keywords = self.keywords().to_vec();
        let lower = keyword.to_lowercase();
        let mut total = self.total;
        if !keywords.iter().any(|known| known.to_lowercase() == lower) {
            keywords.push(keyword.to_owned());
            total += 1;
        }
        let (keywords, _) = shown_keywords(keywords);
        self.selected = keywords
            .iter()
            .position(|known| known == keyword)
            .unwrap_or(self.selected);
        self.list = Some(keywords);
        self.total = total;
        self.reveal = true;
    }

    /// A keyword was removed: it goes at once (the worker's list follows).
    pub fn removed(&mut self, keyword: &str) {
        let mut keywords = self.keywords().to_vec();
        let before = keywords.len();
        keywords.retain(|known| known != keyword);
        let total = self.total.saturating_sub(before - keywords.len());
        self.set_list(keywords, total);
    }

    /// Selects the row at `index`; a question about another row is dropped.
    pub fn select(&mut self, index: usize) {
        if index >= self.keywords().len() {
            return;
        }
        if self.selected != index {
            self.confirm = None;
        }
        self.selected = index;
        self.reveal = true;
    }

    pub fn step(&mut self, delta: isize) {
        let len = self.keywords().len();
        if len == 0 {
            return;
        }
        let to = (self.selected as isize + delta).clamp(0, len as isize - 1);
        self.select(to as usize);
    }

    pub fn scroll_by(&mut self, lines: isize) {
        self.scroll = (self.scroll as isize + lines).max(0) as usize;
        self.reveal = false;
    }
}

/// What was typed, as a keyword: trimmed, and checked for what the server would refuse
/// or what would make no sense. `known` is the list the popup shows, `total` how many
/// the account has. The error is for the user.
pub fn check_keyword(typed: &str, known: &[String], total: usize) -> Result<String, String> {
    let keyword = typed.trim();
    if is_blank(keyword) {
        return Err("Type a word to add.".to_owned());
    }
    // The field leaves them out; a keyword must not bring any to other clients.
    if keyword.chars().any(unsafe_char) {
        return Err("A keyword cannot hold control characters.".to_owned());
    }
    if keyword.chars().count() > MAX_CHARS {
        return Err(format!(
            "A keyword can have at most {MAX_CHARS} characters."
        ));
    }
    // The keyword is also the name of its rule on the server, and names like these
    // are not allowed there.
    if keyword.starts_with('.') {
        return Err("A keyword cannot start with a dot.".to_owned());
    }
    if keyword.contains(['/', '\\']) {
        return Err("A keyword cannot hold / or \\.".to_owned());
    }
    // The server matches keywords whatever their case.
    let lower = keyword.to_lowercase();
    if known.iter().any(|known| known.to_lowercase() == lower) {
        return Err("That keyword is in the list already.".to_owned());
    }
    if total >= MAX_KEYWORDS {
        return Err(format!(
            "There are {MAX_KEYWORDS} keywords already: remove one first."
        ));
    }
    Ok(keyword.to_owned())
}

/// The question before removing `keyword`: the keys first, so that a narrow popup cuts
/// the rest and not them.
pub fn confirm_prompt(keyword: &str) -> String {
    format!(
        "Del again to remove, Esc to keep · “{}” will no longer notify you",
        truncate(&sanitize_line(keyword), 24)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn a_new_keyword_is_trimmed_and_checked() {
        let known = list(&["Lunch", "deploy"]);
        assert_eq!(check_keyword("  release ", &known, 2).unwrap(), "release");
        assert_eq!(check_keyword("two words", &known, 2).unwrap(), "two words");
        for (typed, error) in [
            ("", "Type a word to add."),
            ("   ", "Type a word to add."),
            ("\u{200b}", "Type a word to add."),
            ("lunch", "That keyword is in the list already."),
            (" DEPLOY", "That keyword is in the list already."),
            ("a\u{1b}[31m", "A keyword cannot hold control characters."),
            ("bell\u{7}", "A keyword cannot hold control characters."),
            (
                "right\u{202e}left",
                "A keyword cannot hold control characters.",
            ),
            (".hidden", "A keyword cannot start with a dot."),
            ("a/b", "A keyword cannot hold / or \\."),
            ("a\\b", "A keyword cannot hold / or \\."),
        ] {
            assert_eq!(
                check_keyword(typed, &known, 2).unwrap_err(),
                error,
                "{typed:?}"
            );
        }
        let longest = "é".repeat(MAX_CHARS);
        assert_eq!(check_keyword(&longest, &known, 2).unwrap(), longest);
        assert_eq!(
            check_keyword(&format!("{longest}x"), &known, 2).unwrap_err(),
            "A keyword can have at most 64 characters."
        );
        assert_eq!(
            check_keyword("more", &known, MAX_KEYWORDS).unwrap_err(),
            "There are 100 keywords already: remove one first."
        );
        // The field takes more than the longest keyword, also in four-byte characters:
        // a keyword that is too long is told, not cut without a word.
        let mut field = KeywordsPopup::new(None).input;
        field.paste(&format!("  {}  ", "🐀".repeat(MAX_CHARS + 5)));
        assert!(field.trim().chars().count() > MAX_CHARS);
        assert!(field.len() <= KeywordsPopup::INPUT_MAX_BYTES);
    }

    #[test]
    fn the_selection_follows_its_keyword_and_a_question_about_a_gone_one_is_dropped() {
        let mut popup = KeywordsPopup::new(Some((list(&["alpha", "beta", "gamma"]), 3)));
        popup.step(1);
        assert_eq!(popup.selected_keyword(), Some("beta"));
        popup.confirm = Some("beta".into());
        popup.set_list(list(&["aardvark", "alpha", "beta", "gamma"]), 4);
        assert_eq!(popup.selected_keyword(), Some("beta"));
        assert!(popup.confirm.is_some());
        popup.removed("beta");
        assert_eq!(popup.keywords(), ["aardvark", "alpha", "gamma"]);
        assert_eq!(popup.total, 3);
        assert!(popup.confirm.is_none());
        assert_eq!(popup.selected_keyword(), Some("gamma"));
        popup.added("Beta");
        assert_eq!(popup.keywords(), ["aardvark", "alpha", "Beta", "gamma"]);
        assert_eq!(popup.selected_keyword(), Some("Beta"));
        assert_eq!(popup.total, 4);
        // Moving to another row drops the question.
        popup.confirm = Some("Beta".into());
        popup.step(-1);
        assert!(popup.confirm.is_none());
        popup.step(-10);
        assert_eq!(popup.selected, 0);
    }

    #[test]
    fn debug_output_shows_no_keyword_and_nothing_typed() {
        let mut popup = KeywordsPopup::new(Some((list(&["secret-project"]), 1)));
        popup.input.set("other-secret");
        popup.confirm = Some("secret-project".into());
        popup.pending = Some((3, Pending::Remove("secret-project".into())));
        popup.error = Some("Could not remove secret-project".into());
        let mut state = KeywordsState::default();
        state.demo.push("secret-demo".into());
        let printed = format!("{popup:?} {state:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert!(!format!("{:?}", crate::modal::Modal::Keywords(popup)).contains("secret"));
    }

    #[test]
    fn the_removal_question_keeps_its_keys_first_and_cuts_a_long_keyword() {
        let prompt = confirm_prompt(&format!("bell\u{7}{}", "x".repeat(40)));
        assert!(prompt.starts_with("Del again to remove, Esc to keep · “bellxxx"));
        assert!(!prompt.contains('\u{7}'));
        assert!(prompt.contains("…” will no longer notify you"));
    }
}
