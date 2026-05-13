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
