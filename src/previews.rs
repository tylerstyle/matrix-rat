//! Link previews: what the homeserver says a link is about, kept for the session.
//!
//! The UI decides which links to ask about (only the first link of a message that is on
//! screen, see `ui::timeline`) and this store keeps the answers: one request per address,
//! a negative answer remembered as well, a size cap, and a limit on how many requests
//! are on their way at once. The worker makes the request (`matrix::previews`); with the
//! homeserver the link is revealed to it, which is why the setting is off by default.
//!
//! What comes back is written by whoever runs the linked page: it is untrusted text.
//! It is cleaned here (control characters and direction marks removed, runs of space
//! collapsed, lengths capped) and is only ever drawn as plain text.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::Duration;

use crate::text::{is_blank, sanitize_line};
use crate::time::Instant;

/// Answers (and "nothing to show" answers) kept; the oldest go first.
pub const MAX_ENTRIES: usize = 200;
/// Requests on their way at once.
pub const MAX_IN_FLIGHT: usize = 3;
/// A request the worker never answered (it restarted, say) may be made again after this.
const STALE: Duration = Duration::from_secs(30);
/// Characters kept of a title and of a description.
pub const TITLE_CHARS: usize = 120;
pub const DESCRIPTION_CHARS: usize = 300;
/// Longest address asked about.
const MAX_URL: usize = 2000;

/// What a preview shows: a bold line and some muted text under it.
#[derive(Clone, PartialEq, Eq)]
pub struct Preview {
    pub title: String,
    pub description: String,
}

impl fmt::Debug for Preview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Preview")
    }
}

/// The three fields of the server's answer that are used, as sent.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Raw {
    pub title: Option<String>,
    pub site: Option<String>,
    pub description: Option<String>,
}

impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Raw")
    }
}

/// Whether an address may be asked about: a web address (nothing else is worth the
/// server's trouble, and `mailto:` or `matrix:` are not fetched), with a host, without
/// a password in it, and not a `matrix.to` link, which is a link into Matrix.
pub fn eligible(address: &str) -> bool {
    let Ok(url) = url::Url::parse(address) else {
        return false;
    };
    address.len() <= MAX_URL
        && matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host != "matrix.to"
        })
        && url.username().is_empty()
        && url.password().is_none()
}

/// One line of untrusted text, safe to draw.
fn clean_line(text: &str, cap: usize) -> String {
    let text = sanitize_line(text);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if is_blank(&text) {
        return String::new();
    }
    if text.chars().count() <= cap {
        return text;
    }
    let mut cut: String = text.chars().take(cap.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// The preview to show for what the server answered, or `None` when there is nothing
/// worth a row.
fn compose(raw: Raw) -> Option<Preview> {
    let title = raw
        .title
        .map(|t| clean_line(&t, TITLE_CHARS))
        .unwrap_or_default();
    let site = raw.site.map(|s| clean_line(&s, 40)).unwrap_or_default();
    let description = raw
        .description
        .map(|d| clean_line(&d, DESCRIPTION_CHARS))
        .unwrap_or_default();
    let title = match (site.is_empty(), title.is_empty()) {
        (false, false) if title.to_lowercase().contains(&site.to_lowercase()) => title,
        (false, false) => format!("{site} · {title}"),
        (false, true) => site,
        (true, _) => title,
    };
    match (title.is_empty(), description.is_empty()) {
        (true, true) => None,
        // Only a description: it is the line that says something.
        (true, false) => Some(Preview {
            title: description,
            description: String::new(),
        }),
        (false, _) => Some(Preview { title, description }),
    }
}

#[derive(Debug)]
enum Slot {
    /// Asked for at this time.
    Loading(Instant),
    Ready(Preview),
    /// Asked, and nothing came of it (no such feature, an error, an empty page).
    Nothing,
}

#[derive(Default)]
pub struct PreviewStore {
    slots: HashMap<String, Slot>,
    /// Addresses in the order they were first asked about.
    order: VecDeque<String>,
    /// Requests made and not yet taken by the worker's sender.
    queue: Vec<String>,
    loading: usize,
    /// Changes when a preview appears or goes: the timeline lays out again.
    pub rev: u64,
}

/// Hand-written: the addresses are links from messages.
impl fmt::Debug for PreviewStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreviewStore")
            .field("slots", &self.slots.len())
            .field("queue", &self.queue.len())
            .field("loading", &self.loading)
            .field("rev", &self.rev)
            .finish_non_exhaustive()
    }
}

impl PreviewStore {
    /// Asks for a preview unless there is one, or one is on its way, or too many
    /// requests are. Called for each link that is on screen, on every frame it is.
    pub fn request(&mut self, address: &str) -> bool {
        self.request_at(address, Instant::now())
    }

    fn request_at(&mut self, address: &str, now: Instant) -> bool {
        match self.slots.get_mut(address) {
            Some(Slot::Loading(since)) if now.duration_since(*since) >= STALE => {
                *since = now;
                self.queue.push(address.to_owned());
                return true;
            }
            Some(_) => return false,
            None => {}
        }
        if self.loading >= MAX_IN_FLIGHT || !eligible(address) {
            return false;
        }
        self.slots.insert(address.to_owned(), Slot::Loading(now));
        self.order.push_back(address.to_owned());
        self.loading += 1;
        self.queue.push(address.to_owned());
        self.evict();
        true
    }

    /// The requests made since the last call, for the worker.
    pub fn take_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.queue)
    }

    /// The worker's answer. Anything that was not asked for (or no longer is, after
    /// [`PreviewStore::clear`], or was answered already) is ignored. True if a preview
    /// is now to be shown.
    pub fn on_loaded(&mut self, address: &str, raw: Option<Raw>) -> bool {
        if !matches!(self.slots.get(address), Some(Slot::Loading(_))) {
            return false;
        }
        self.loading = self.loading.saturating_sub(1);
        let preview = raw.and_then(compose);
        let shown = preview.is_some();
        self.slots.insert(
            address.to_owned(),
            preview.map_or(Slot::Nothing, Slot::Ready),
        );
        if shown {
            self.rev += 1;
        }
        shown
    }

    pub fn get(&self, address: &str) -> Option<&Preview> {
        match self.slots.get(address) {
            Some(Slot::Ready(preview)) => Some(preview),
            _ => None,
        }
    }

    /// Forgets everything (another account, or logging out). What is still on its way
    /// is ignored when it arrives.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.order.clear();
        self.queue.clear();
        self.loading = 0;
        self.rev += 1;
    }

    /// Keeps to [`MAX_ENTRIES`], dropping the oldest that are not waiting for an answer.
    fn evict(&mut self) {
        while self.slots.len() > MAX_ENTRIES {
            let Some(at) = self
                .order
                .iter()
                .position(|address| !matches!(self.slots.get(address), Some(Slot::Loading(_))))
            else {
                break;
            };
            let Some(address) = self.order.remove(at) else {
                break;
            };
            if matches!(self.slots.remove(&address), Some(Slot::Ready(_))) {
                self.rev += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(title: &str, site: &str, description: &str) -> Raw {
        let some = |s: &str| (!s.is_empty()).then(|| s.to_owned());
        Raw {
            title: some(title),
            site: some(site),
            description: some(description),
        }
    }

    #[test]
    fn only_web_addresses_are_asked_about() {
        for good in [
            "https://example.org/a?b=c#d",
            "http://example.org",
            "https://sub.example.org:8448/x",
        ] {
            assert!(eligible(good), "{good}");
        }
        for bad in [
            "matrix:r/room:example.org",
            "https://matrix.to/#/@a:example.org",
            "https://MATRIX.TO./#/!r:example.org",
            "mailto:a@example.org",
            "javascript:alert(1)",
            "ftp://example.org/file",
            "https://user:pw@example.org/",
            "https://user@example.org/",
            "file:///etc/passwd",
            "not a url",
            "https://",
            "",
        ] {
            assert!(!eligible(bad), "{bad}");
        }
        assert!(!eligible(&format!(
            "https://example.org/{}",
            "a".repeat(2100)
        )));
        // Something that merely mentions it is another site.
        assert!(eligible("https://matrix.to.example.org/"));
    }

    #[test]
    fn what_the_server_says_is_cleaned_and_capped() {
        let preview = compose(raw(
            "  Hello\u{1b}[2J \u{202e}world\n\ttwo  ",
            "",
            "line one\nline\u{0}two \u{200f}",
        ))
        .unwrap();
        assert_eq!(preview.title, "Hello[2J world two");
        assert_eq!(preview.description, "line one linetwo");

        let long = compose(raw(&"t".repeat(500), "", &"d ".repeat(500))).unwrap();
        assert_eq!(long.title.chars().count(), TITLE_CHARS);
        assert!(long.title.ends_with('…'));
        assert!(long.description.chars().count() <= DESCRIPTION_CHARS);
        assert!(long.description.ends_with('…'));
        // Markup stays text.
        let markup = compose(raw("<b>x</b> **y** [z](http://a)", "", "")).unwrap();
        assert_eq!(markup.title, "<b>x</b> **y** [z](http://a)");
    }

    #[test]
    fn site_title_and_description_make_up_the_lines() {
        let both = compose(raw("A page", "Example", "About it")).unwrap();
        assert_eq!(
            (both.title.as_str(), both.description.as_str()),
            ("Example · A page", "About it")
        );
        // A title that has the site's name already is not doubled.
        assert_eq!(
            compose(raw("Example: a page", "example", ""))
                .unwrap()
                .title,
            "Example: a page"
        );
        assert_eq!(compose(raw("", "Example", "x")).unwrap().title, "Example");
        // Only a description is the bold line.
        let only = compose(raw("", "", "Just this")).unwrap();
        assert_eq!(
            (only.title.as_str(), only.description.as_str()),
            ("Just this", "")
        );
        // Nothing, or nothing that shows.
        assert_eq!(compose(raw("", "", "")), None);
        assert_eq!(compose(raw("\u{200b}\u{200b}", "\u{1b}", " \n ")), None);
    }

    #[test]
    fn each_address_is_asked_once_and_failures_are_remembered() {
        let mut store = PreviewStore::default();
        let url = "https://example.org/a";
        assert!(store.request(url));
        assert!(!store.request(url));
        assert_eq!(store.take_requests(), [url]);
        assert!(store.take_requests().is_empty());
        // No answer: nothing to show, and not asked again.
        assert!(!store.on_loaded(url, None));
        assert!(store.get(url).is_none());
        assert!(!store.request(url));
        assert!(store.take_requests().is_empty());

        let other = "https://example.org/b";
        assert!(store.request(other));
        let rev = store.rev;
        assert!(store.on_loaded(other, Some(raw("T", "", ""))));
        assert!(store.rev > rev);
        assert_eq!(store.get(other).unwrap().title, "T");
        assert!(!store.request(other));
        // An answer that shows nothing does not make the timeline lay out again.
        let rev = store.rev;
        store.request("https://example.org/c");
        assert!(!store.on_loaded("https://example.org/c", Some(raw("", "", ""))));
        assert_eq!(store.rev, rev);
    }

    #[test]
    fn only_a_few_requests_are_on_their_way() {
        let mut store = PreviewStore::default();
        let urls: Vec<String> = (0..6).map(|n| format!("https://example.org/{n}")).collect();
        let started: Vec<bool> = urls.iter().map(|url| store.request(url)).collect();
        assert_eq!(started, [true, true, true, false, false, false]);
        assert_eq!(store.take_requests().len(), MAX_IN_FLIGHT);
        // One answer makes room; the ones refused are asked for again on a later frame.
        store.on_loaded(&urls[0], None);
        assert!(store.request(&urls[3]));
        assert!(!store.request(&urls[4]));
        // An address that cannot be asked about does not use a slot.
        let mut store = PreviewStore::default();
        assert!(!store.request("https://matrix.to/#/@a:b"));
        assert!(!store.request("matrix:r/x:y"));
        assert!(store.take_requests().is_empty());
    }

    #[test]
    fn answers_nobody_waits_for_are_ignored() {
        let mut store = PreviewStore::default();
        // Never asked.
        assert!(!store.on_loaded("https://example.org/x", Some(raw("T", "", ""))));
        assert!(store.get("https://example.org/x").is_none());
        // Asked, then forgotten (logout): the late answer is dropped, and the slot count
        // is not thrown off.
        let url = "https://example.org/a";
        store.request(url);
        store.clear();
        assert!(!store.on_loaded(url, Some(raw("T", "", ""))));
        assert!(store.get(url).is_none());
        for n in 0..MAX_IN_FLIGHT {
            assert!(store.request(&format!("https://example.org/{n}")));
        }
        // A second answer for one that was answered changes nothing.
        let first = "https://example.org/0";
        assert!(store.on_loaded(first, Some(raw("T", "", ""))));
        assert!(!store.on_loaded(first, Some(raw("Other", "", ""))));
        assert_eq!(store.get(first).unwrap().title, "T");
    }

    #[test]
    fn a_request_that_never_came_back_is_made_again_after_a_while() {
        let mut store = PreviewStore::default();
        let url = "https://example.org/a";
        let start = Instant::now();
        assert!(store.request_at(url, start));
        store.take_requests();
        assert!(!store.request_at(url, start + Duration::from_secs(10)));
        assert!(store.request_at(url, start + STALE));
        assert_eq!(store.take_requests(), [url]);
        // Still one slot, not two.
        assert_eq!(store.loading, 1);
    }

    #[test]
    fn the_store_has_a_size_cap_and_keeps_what_is_waiting() {
        let mut store = PreviewStore::default();
        for n in 0..MAX_ENTRIES + 50 {
            let url = format!("https://example.org/{n}");
            assert!(store.request(&url), "{n}");
            store.on_loaded(&url, (n % 2 == 0).then(|| raw("T", "", "")));
        }
        assert_eq!(store.slots.len(), MAX_ENTRIES);
        // The oldest are gone, the newest are there.
        assert!(store.get("https://example.org/0").is_none());
        let last = format!("https://example.org/{}", MAX_ENTRIES + 48);
        assert!(store.get(&last).is_some());
        // One that is waiting is never dropped for the cap.
        let waiting = "https://example.org/waiting";
        assert!(store.request(waiting));
        for n in 0..MAX_ENTRIES {
            let url = format!("https://example.org/again-{n}");
            // Only two more can be on their way next to the waiting one.
            if store.request(&url) {
                store.on_loaded(&url, None);
            }
        }
        assert!(store.on_loaded(waiting, Some(raw("late", "", ""))));
    }

    #[test]
    fn nothing_of_a_preview_is_in_debug_output() {
        let preview = compose(raw("secret title", "", "secret text")).unwrap();
        assert!(!format!("{preview:?}").contains("secret"));
        assert!(!format!("{:?}", raw("secret title", "", "")).contains("secret"));
    }
}
