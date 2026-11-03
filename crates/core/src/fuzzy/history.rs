//! Prompt-history search behind `smelt.history.search`.
//!
//! Each query word must appear in the entry. Matches rank by tier, best
//! first: the whole entry, an entry prefix, every word at a word start, every
//! word as a substring, then a fuzzy match on the first line. Within a tier,
//! matches visible in the picker row beat matches further along the first
//! line, which beat matches on later lines; then newer entries beat older
//! ones. Case is smart: an uppercase query letter makes the search
//! case-sensitive.

use std::collections::HashSet;

use smelt_buffer::text;

use super::{matcher, respects_case, CaseMatching};

/// Leading characters of the first line that a picker row shows.
const VISIBLE_CHARS: usize = 80;

/// Deduplicated, case-folded history entries, newest first. Built once per
/// history revision so a keystroke only scans.
pub struct HistoryIndex {
    entries: Vec<IndexedEntry>,
}

struct IndexedEntry {
    /// Position in the oldest-first history.
    index: usize,
    /// Entry with surrounding whitespace trimmed; its first line is the label.
    text: Box<str>,
    folded: Box<str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Exact,
    Prefix,
    WordStart,
    Substring,
    Fuzzy,
}

/// Where the furthest matched text of an entry ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Placement {
    /// Within the leading characters a picker row shows.
    Visible,
    /// Further along the first line.
    Label,
    /// On a later line.
    Body,
}

/// Sort key of one match; lower is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    tier: Tier,
    placement: Placement,
    /// Inverted fuzzy score; zero for literal tiers.
    fuzzy: u16,
    /// Position in the newest-first index.
    recency: usize,
}

impl HistoryIndex {
    /// Index `entries`, given oldest first.
    pub fn build<'a>(
        entries: impl DoubleEndedIterator<Item = &'a str> + ExactSizeIterator,
    ) -> Self {
        let total = entries.len();
        let mut seen = HashSet::with_capacity(total);
        let mut indexed = Vec::with_capacity(total);
        for (index, entry) in entries.enumerate().rev() {
            let trimmed = text::trim_whitespace(entry);
            if trimmed.is_empty() || !seen.insert(trimmed) {
                continue;
            }
            indexed.push(IndexedEntry {
                index,
                text: trimmed.into(),
                folded: trimmed.to_lowercase().into(),
            });
        }
        Self { entries: indexed }
    }

    /// Up to `limit` history positions (oldest-first indices) matching
    /// `query`, best first. An empty query lists the newest entries.
    pub fn search(&self, query: &str, limit: usize) -> Vec<usize> {
        let _perf = smelt_perf::perf::begin("fuzzy:history_search");
        let query = text::trim_whitespace(query);
        if query.is_empty() {
            return self.entries.iter().take(limit).map(|e| e.index).collect();
        }
        let case_sensitive = respects_case(CaseMatching::Smart, query);
        let folded_query;
        let needle = if case_sensitive {
            query
        } else {
            folded_query = query.to_lowercase();
            folded_query.as_str()
        };
        let words: Vec<&str> = needle.split_whitespace().collect();

        let mut ranked = Vec::new();
        let mut unmatched = Vec::new();
        for (recency, entry) in self.entries.iter().enumerate() {
            let haystack = if case_sensitive {
                &entry.text
            } else {
                &entry.folded
            };
            match literal_rank(haystack, needle, &words) {
                Some((tier, placement)) => ranked.push(Rank {
                    tier,
                    placement,
                    fuzzy: 0,
                    recency,
                }),
                None => unmatched.push(recency),
            }
        }

        // Fuzzy hits rank below every literal hit, so skip them once the
        // literal hits fill the page.
        if ranked.len() < limit && !unmatched.is_empty() {
            let labels: Vec<&str> = unmatched
                .iter()
                .map(|&recency| first_line(&self.entries[recency].text))
                .collect();
            for m in matcher(query, CaseMatching::Smart).match_list(&labels) {
                ranked.push(Rank {
                    tier: Tier::Fuzzy,
                    placement: Placement::Label,
                    fuzzy: u16::MAX - m.score,
                    recency: unmatched[m.index as usize],
                });
            }
        }

        ranked.sort_unstable();
        ranked
            .into_iter()
            .take(limit)
            .map(|rank| self.entries[rank.recency].index)
            .collect()
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// Byte offsets where the visible part and the whole of the first line end.
struct Label {
    visible_end: usize,
    end: usize,
}

impl Label {
    fn of(haystack: &str) -> Self {
        let end = haystack.find('\n').unwrap_or(haystack.len());
        let visible_end = text::slice(haystack, 0..end)
            .char_indices()
            .nth(VISIBLE_CHARS)
            .map_or(end, |(pos, _)| pos);
        Self { visible_end, end }
    }

    fn placement(&self, match_end: usize) -> Placement {
        if match_end <= self.visible_end {
            Placement::Visible
        } else if match_end <= self.end {
            Placement::Label
        } else {
            Placement::Body
        }
    }
}

/// Literal tier of `haystack` against the whole `needle` and its `words`,
/// plus where the furthest matched word ends.
fn literal_rank(haystack: &str, needle: &str, words: &[&str]) -> Option<(Tier, Placement)> {
    let label = Label::of(haystack);
    if haystack == needle {
        return Some((Tier::Exact, label.placement(needle.len())));
    }
    if haystack.starts_with(needle) {
        return Some((Tier::Prefix, label.placement(needle.len())));
    }
    let mut tier = Tier::WordStart;
    let mut placement = Placement::Visible;
    for word in words {
        let (word_tier, word_placement) = word_rank(haystack, word, &label)?;
        tier = tier.max(word_tier);
        placement = placement.max(word_placement);
    }
    Some((tier, placement))
}

/// Best occurrence of `word` in `haystack`: word starts beat mid-word hits,
/// then earlier placements beat later ones.
fn word_rank(haystack: &str, word: &str, label: &Label) -> Option<(Tier, Placement)> {
    let mut best: Option<(Tier, Placement)> = None;
    for (pos, _) in haystack.match_indices(word) {
        let tier = if is_word_start(haystack, pos, word) {
            Tier::WordStart
        } else {
            Tier::Substring
        };
        let candidate = (tier, label.placement(pos + word.len()));
        if best.is_none_or(|best| candidate < best) {
            best = Some(candidate);
        }
        if candidate == (Tier::WordStart, Placement::Visible) {
            break;
        }
    }
    best
}

fn is_word_start(haystack: &str, pos: usize, word: &str) -> bool {
    !word.starts_with(char::is_alphanumeric)
        || text::slice(haystack, 0..pos)
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::HistoryIndex;

    /// Search `entries` (oldest first, like the live history) and return the
    /// matching entries best first.
    fn search<'a>(entries: &[&'a str], query: &str) -> Vec<&'a str> {
        HistoryIndex::build(entries.iter().copied())
            .search(query, usize::MAX)
            .into_iter()
            .map(|index| entries[index])
            .collect()
    }

    #[test]
    fn empty_query_lists_newest_first_without_duplicates() {
        assert_eq!(search(&["a", "b", "a", "  "], ""), ["a", "b"]);
    }

    #[test]
    fn exact_entry_beats_prefix_beats_word() {
        let entries = ["please commit", "commit amend", "commit"];
        assert_eq!(
            search(&entries, "commit"),
            ["commit", "commit amend", "please commit"]
        );
    }

    #[test]
    fn word_start_beats_mid_word() {
        assert_eq!(
            search(&["recommit later", "run commit hooks"], "commit"),
            ["run commit hooks", "recommit later"]
        );
    }

    #[test]
    fn word_prefix_counts_as_word_start() {
        assert_eq!(
            search(&["the fuzz/README.md", "rerun fuzzing"], "fuzz"),
            ["rerun fuzzing", "the fuzz/README.md"]
        );
    }

    #[test]
    fn newer_entry_wins_within_a_tier() {
        assert_eq!(
            search(&["older bun prompt", "newer bun prompt"], "bun"),
            ["newer bun prompt", "older bun prompt"]
        );
    }

    #[test]
    fn every_query_word_must_match_in_any_order() {
        assert_eq!(
            search(
                &["open the resume dialog", "dialog for resume", "resume only"],
                "resume dialog"
            ),
            ["dialog for resume", "open the resume dialog"]
        );
    }

    #[test]
    fn matches_visible_in_the_row_beat_later_first_line_matches() {
        let long = format!("{} fuzz", "word ".repeat(20));
        let entries = ["rerun fuzz", long.as_str()];
        assert_eq!(search(&entries, "fuzz"), [entries[0], entries[1]]);
    }

    #[test]
    fn later_lines_match_after_first_line_hits() {
        let entries = ["fix this:\n\nthe resume dialog lags", "resume dialog lags"];
        assert_eq!(
            search(&entries, "dialog"),
            ["resume dialog lags", "fix this:\n\nthe resume dialog lags"]
        );
    }

    #[test]
    fn fuzzy_first_line_matches_rank_last() {
        assert_eq!(
            search(&["comet", "commit it", "explain commit"], "cmit"),
            ["commit it", "explain commit"]
        );
        assert_eq!(
            search(&["xcmit", "commit it"], "cmit"),
            ["xcmit", "commit it"]
        );
    }

    #[test]
    fn fuzzy_ignores_text_past_the_first_line() {
        assert!(search(&["hello\nc o m m i t"], "cmit").is_empty());
    }

    #[test]
    fn smart_case_turns_sensitive_with_uppercase() {
        let entries = ["readme fix", "README fix"];
        assert_eq!(search(&entries, "readme"), ["README fix", "readme fix"]);
        assert_eq!(search(&entries, "README"), ["README fix"]);
    }

    #[test]
    fn limit_caps_results() {
        let index = HistoryIndex::build(["a1", "a2", "a3"].into_iter());
        assert_eq!(index.search("a", 2), [2, 1]);
    }

    #[test]
    fn non_ascii_text_matches_without_panicking() {
        assert_eq!(search(&["café crème", "naïve"], "crème"), ["café crème"]);
        assert_eq!(search(&["ÉTÉ chaud"], "été"), ["ÉTÉ chaud"]);
    }
}
