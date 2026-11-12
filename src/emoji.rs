//! Emoji lookup for the picker.

use emojis::Emoji;
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

#[cfg(test)]
mod tests {
    use super::*;

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
