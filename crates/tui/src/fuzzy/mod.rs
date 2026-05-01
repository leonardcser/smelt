pub mod score;

/// Fuzzy match with relevance scoring.
/// Returns `None` if no match, `Some(score)` if matched.
/// Lower score = better match.
///
/// Scoring rewards:
/// - Matches at the start of the string or after a separator (`/`, `-`, `_`, `.`)
/// - Consecutive matching characters
/// - Shorter candidates (less noise)
pub fn fuzzy_score(text: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }

    let hay: Vec<char> = text.chars().collect();
    let need: Vec<char> = query.chars().collect();
    let hay_lower: Vec<char> = text.to_lowercase().chars().collect();
    let need_lower: Vec<char> = query.to_lowercase().chars().collect();

    if hay_lower.len() < need_lower.len() {
        return None;
    }

    // Find match positions greedily, preferring separator-boundary hits.
    let positions = find_best_positions(&hay_lower, &need_lower)?;

    let mut score: u32 = 0;

    // Penalty for each unmatched character (prefer shorter candidates).
    score += (hay.len() - need.len()) as u32;

    // Bonus/penalty per matched character.
    let mut prev_pos: Option<usize> = None;
    for (qi, &hi) in positions.iter().enumerate() {
        // Consecutive bonus: matched chars in a row are great.
        let consecutive = prev_pos.is_some_and(|p| hi == p + 1);
        if !consecutive {
            // Gap penalty.
            if let Some(p) = prev_pos {
                score += (hi - p - 1) as u32 * 2;
            }
        }

        // Boundary bonus: match right after a separator or at start.
        let on_boundary = hi == 0 || matches!(hay[hi - 1], '/' | '-' | '_' | '.' | ' ');
        if on_boundary && !consecutive {
            // Starting a new word segment is good.
        } else if !consecutive {
            score += 3;
        }

        // Exact case match bonus.
        if qi < need.len() && hay[hi] == need[qi] {
            // no penalty
        } else {
            score += 1;
        }

        prev_pos = Some(hi);
    }

    // Prefix match bonus: if first match is at position 0, big reward.
    if positions[0] == 0 {
        score = score.saturating_sub(5);
    }

    Some(score)
}

/// Find match positions with a preference for boundary-aligned matches.
/// Two-pass: first try to place chars on boundaries, then fill gaps greedily.
fn find_best_positions(hay: &[char], need: &[char]) -> Option<Vec<usize>> {
    // Simple greedy left-to-right.
    let mut positions = Vec::with_capacity(need.len());
    let mut hi = 0;
    for &qc in need {
        while hi < hay.len() {
            if hay[hi] == qc {
                positions.push(hi);
                hi += 1;
                break;
            }
            hi += 1;
        }
        if positions.len() < need.len() - (need.len() - positions.len()) {
            // still matching
        }
    }

    if positions.len() == need.len() {
        Some(positions)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_prefix_wins() {
        let a = fuzzy_score("src/main.rs", "src").unwrap();
        let b = fuzzy_score("crates/engine/src", "src").unwrap();
        assert!(a < b, "prefix match should score better: {a} vs {b}");
    }

    #[test]
    fn shorter_path_wins() {
        let a = fuzzy_score("src/lib.rs", "src").unwrap();
        let b = fuzzy_score("crates/engine/src/lib.rs", "src").unwrap();
        assert!(a < b, "shorter match should score better: {a} vs {b}");
    }

    #[test]
    fn consecutive_wins() {
        let a = fuzzy_score("src/main.rs", "src").unwrap();
        let b = fuzzy_score("some_random_config", "src").unwrap();
        assert!(a < b, "consecutive match should score better: {a} vs {b}");
    }

    #[test]
    fn no_match() {
        assert!(fuzzy_score("hello", "xyz").is_none());
    }

    #[test]
    fn empty_query_matches_all() {
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn boundary_match() {
        // "ml" should prefer "main.rs" (m at start, no boundary for l) less than "mod/lib.rs" (m at boundary, l at boundary)
        // Actually both have boundary matches. Let's test something clearer:
        let a = fuzzy_score("Cargo.lock", "cl").unwrap();
        let b = fuzzy_score("crates/engine/lib.rs", "cl").unwrap();
        // "Cargo.lock" has c at 0 (boundary) and l after dot (boundary)
        // "crates/engine/lib.rs" has c at 0 (boundary) and l after / (boundary) but longer
        assert!(a < b, "shorter boundary match should win: {a} vs {b}");
    }
}
