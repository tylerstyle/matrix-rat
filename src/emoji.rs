//! Emoji lookup for the picker.

use emojis::Emoji;

use crate::matrix::CustomEmoji;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// All emoji in their standard order for an empty query, otherwise the fuzzy matches on
/// name and shortcodes (`:thumbsup:` style), best first.
pub fn search(query: &str) -> Vec<&'static Emoji> {
    let query = query.trim().trim_matches(':');
    if query.is_empty() {
        return emojis::iter().collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buffer = Vec::new();
    let lowered = query.to_lowercase();
    // Ranking: exact match, then prefix, then "contains", then typo-tolerant fuzzy hits.
    // Fuzzy-only hits are noisy ("cat" matches "rotating light"), so they are a
    // fallback for when little else was found.
    const EXACT: u32 = 30_000;
    const PREFIX: u32 = 20_000;
    const CONTAINS: u32 = 10_000;
    let mut score = |text: &str| -> Option<u32> {
        let fuzzy = pattern.score(Utf32Str::new(text, &mut buffer), &mut matcher)?;
        let text = text.to_lowercase();
        let shorter_is_better = 1_000u32.saturating_sub(text.len() as u32);
        Some(if text == lowered {
            EXACT
        } else if text.starts_with(&lowered) {
            PREFIX + shorter_is_better
        } else if text.contains(&lowered) {
            CONTAINS + shorter_is_better
        } else {
            fuzzy.min(CONTAINS - 1)
        })
    };

    let mut scored: Vec<(u32, &'static Emoji)> = emojis::iter()
        .filter_map(|emoji| {
            let best = emoji
                .shortcodes()
                .chain(std::iter::once(emoji.name()))
                .filter_map(&mut score)
                .max()?;
            Some((best, emoji))
        })
        .collect();
    const ENOUGH: usize = 8;
    if scored.iter().filter(|(s, _)| *s >= CONTAINS).count() >= ENOUGH {
        scored.retain(|(s, _)| *s >= CONTAINS);
    }
    // Stable: equal scores keep the standard emoji order.
    scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    scored.into_iter().map(|(_, emoji)| emoji).collect()
}

/// The custom emoji (indices into `custom`) that match `query`, best first: the name
/// itself, names starting with it, names containing it, then those whose description or
/// pack has it. Every word typed has to be found somewhere. All of them, in their
/// order, for an empty query.
pub fn search_custom(query: &str, custom: &[CustomEmoji]) -> Vec<usize> {
    let query = query.trim().trim_matches(':').to_lowercase();
    if query.is_empty() {
        return (0..custom.len()).collect();
    }
    let mut scored: Vec<(u8, usize)> = custom
        .iter()
        .enumerate()
        .filter_map(|(at, emoji)| {
            let code = emoji.shortcode.to_lowercase();
            let body = emoji.body.as_deref().unwrap_or_default().to_lowercase();
            let pack = emoji.pack_name.to_lowercase();
            let score = if code == query {
                4
            } else if code.starts_with(&query) {
                3
            } else if code.contains(&query) {
                2
            } else if body.contains(&query) {
                1
            } else if query
                .split_whitespace()
                .all(|word| code.contains(word) || body.contains(word) || pack.contains(word))
            {
                0
            } else {
                return None;
            };
            Some((score, at))
        })
        .collect();
    // Stable: equal scores keep the packs' order.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored.into_iter().map(|(_, at)| at).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_emoji_are_found_by_name_description_and_pack() {
        let emoji = |code: &str, body: Option<&str>, pack: &str| CustomEmoji {
            shortcode: code.into(),
            url: format!("mxc://example.org/{code}"),
            body: body.map(str::to_owned),
            pack_name: pack.into(),
        };
        let custom = [
            emoji("partyblob", None, "Blobs"),
            emoji("blob", None, "Blobs"),
            emoji("catjam", Some("A cat that dances"), "Cats"),
            emoji("blobwave", None, "Blobs"),
        ];
        assert_eq!(search_custom("", &custom), [0, 1, 2, 3]);
        // The name itself, then names starting with it, then names containing it.
        assert_eq!(search_custom(":blob:", &custom), [1, 3, 0]);
        assert_eq!(search_custom("BLOBW", &custom), [3]);
        // A description or a pack counts as well, after the names.
        assert_eq!(search_custom("dances", &custom), [2]);
        assert_eq!(search_custom("cats jam", &custom), [2]);
        assert!(search_custom("zzz", &custom).is_empty());
    }

    #[test]
    fn empty_query_lists_everything_in_standard_order() {
        let all = search("");
        assert!(all.len() > 1000);
        assert_eq!(all[0].as_str(), "😀");
        assert_eq!(search("  ").len(), all.len());
    }

    #[test]
    fn finds_by_name_and_shortcode() {
        assert_eq!(search("thumbsup")[0].as_str(), "👍");
        assert_eq!(search(":rat:")[0].as_str(), "🐀");
        assert_eq!(search("heart")[0].as_str(), "❤️");
        assert!(search("zzzzqqqq").is_empty());
        // Plenty of real matches: no fuzzy noise.
        let cats = search("cat");
        assert_eq!(cats[0].as_str(), "🐱");
        assert!(cats.iter().all(|e| {
            e.shortcodes()
                .chain(std::iter::once(e.name()))
                .any(|t| t.to_lowercase().contains("cat"))
        }));
    }

    #[test]
    fn is_fuzzy_and_case_insensitive() {
        assert!(search("thmbsup").iter().take(5).any(|e| e.as_str() == "👍"));
        assert_eq!(search("RAT")[0].as_str(), "🐀");
    }
}
