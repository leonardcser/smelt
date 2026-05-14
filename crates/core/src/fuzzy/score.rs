pub fn split_words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

/// Recency bonus for history items (newest = rank 0).
pub fn recency_bonus(recency_rank: usize) -> i64 {
    match recency_rank {
        0..=4 => 180 - (recency_rank as i64 * 20),
        5..=14 => 90 - ((recency_rank as i64 - 5) * 6),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_words_splits_on_non_alphanumeric() {
        assert_eq!(split_words("foo bar"), vec!["foo", "bar"]);
        assert_eq!(split_words("foo_bar-baz"), vec!["foo", "bar", "baz"]);
        assert_eq!(split_words("camelCase123"), vec!["camelCase123"]);
        assert_eq!(split_words("a/b\\c.d"), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn split_words_drops_empty_segments() {
        assert_eq!(split_words("  foo   bar  "), vec!["foo", "bar"]);
        assert_eq!(split_words(""), Vec::<&str>::new());
        assert_eq!(split_words("///"), Vec::<&str>::new());
    }

    #[test]
    fn recency_bonus_rewards_newest_most() {
        assert_eq!(recency_bonus(0), 180);
        assert_eq!(recency_bonus(1), 160);
        assert_eq!(recency_bonus(4), 100);
    }

    #[test]
    fn recency_bonus_decays_through_middle_tier() {
        assert_eq!(recency_bonus(5), 90);
        assert_eq!(recency_bonus(14), 36);
    }

    #[test]
    fn recency_bonus_zero_beyond_fifteen() {
        assert_eq!(recency_bonus(15), 0);
        assert_eq!(recency_bonus(100), 0);
    }

    #[test]
    fn recency_bonus_is_monotonically_non_increasing() {
        let prev = (0..30).map(recency_bonus).collect::<Vec<_>>();
        for w in prev.windows(2) {
            assert!(w[0] >= w[1], "non-monotonic at {w:?}");
        }
    }
}
