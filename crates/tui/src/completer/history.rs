use std::collections::HashSet;

use super::score::{recency_bonus, split_words};
use super::{Completer, CompleterKind, CompletionItem};

impl Completer {
    pub fn history(entries: &[String]) -> Self {
        let mut seen = HashSet::new();
        let all_items: Vec<CompletionItem> = entries
            .iter()
            .rev()
            .filter(|text| seen.insert(text.as_str()))
            .map(|text| {
                let label = text
                    .trim_start()
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("")
                    .to_string();
                CompletionItem {
                    label,
                    ..Default::default()
                }
            })
            .collect();
        let results = all_items.clone();
        Self {
            anchor: 0,
            kind: CompleterKind::History,
            query: String::new(),
            results,
            selected: 0,
            all_items,
            selected_key: None,
            original_value: None,
        }
    }
}

pub(super) fn history_score(text: &str, query: &str, recency_rank: usize) -> Option<u32> {
    let base = crate::fuzzy::fuzzy_score(text, query)? as i64;
    let text_norm = text.trim().to_lowercase();
    let query_norm = query.trim().to_lowercase();

    if query_norm.is_empty() {
        return Some(recency_rank as u32);
    }

    let text_words = split_words(&text_norm);
    let query_words = split_words(&query_norm);
    let query_has_multiple_words = query_words.len() > 1;

    let mut score = base * 10;

    if text_norm == query_norm {
        score -= 2_000;
    } else if text_norm.starts_with(&query_norm) {
        score -= 200;
    }

    if !query_has_multiple_words && text_words.len() > 1 {
        score += ((text_words.len() - 1) as i64) * 60;
    }

    let mut saw_exact_word_match = false;
    let mut saw_prefix_word_match = false;
    let mut saw_substring_match = false;

    for word in &query_words {
        if text_words.iter().any(|candidate| candidate == word) {
            saw_exact_word_match = true;
            score -= 400;
        } else if text_words
            .iter()
            .any(|candidate| candidate.starts_with(word))
        {
            saw_prefix_word_match = true;
            score -= 140;
        } else if text_norm.contains(word) {
            saw_substring_match = true;
            score -= 40;
        }
    }

    if !query_has_multiple_words {
        if let Some(first_word) = query_words.first() {
            let boundary_prefix_matches = text_words
                .iter()
                .filter(|candidate| candidate.starts_with(first_word))
                .count();
            if boundary_prefix_matches > 0 {
                score -= 80;
            }
        }
    }

    if !query_has_multiple_words {
        // For single-word reverse search, plain fuzzy subsequence matches like
        // "default allow" for "full" should come well after true word hits.
        if !saw_exact_word_match && !saw_prefix_word_match && !saw_substring_match {
            score += 900;
        }
    }

    score -= recency_bonus(recency_rank);

    Some(score.max(0) as u32)
}
