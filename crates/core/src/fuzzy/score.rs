pub fn query_match_score(query: &str, query_words: &[&str], fields: &[String]) -> Option<u32> {
    let field_words: Vec<Vec<&str>> = fields.iter().map(|field| split_words(field)).collect();
    let exact_primary = field_words
        .first()
        .is_some_and(|words| words.contains(&query));
    let exact_secondary = field_words
        .iter()
        .skip(1)
        .any(|words| words.contains(&query));
    let prefix_primary = field_words
        .first()
        .is_some_and(|words| words.iter().any(|word| word.starts_with(query)));
    let prefix_secondary = field_words
        .iter()
        .skip(1)
        .any(|words| words.iter().any(|word| word.starts_with(query)));
    let all_words_match = !query_words.is_empty()
        && query_words.iter().all(|word| {
            field_words
                .iter()
                .any(|words| words.iter().any(|candidate| candidate.starts_with(word)))
        });

    if exact_primary {
        return Some(0);
    }
    if exact_secondary {
        return Some(1);
    }
    if prefix_primary {
        return Some(2);
    }
    if prefix_secondary {
        return Some(3);
    }
    if all_words_match {
        return Some(4);
    }

    let haystack = fields.join(" ");
    crate::fuzzy::fuzzy_score(&haystack, query).map(|score| score + 100)
}

pub fn split_words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

pub fn recency_bonus(recency_rank: usize) -> i64 {
    // History items are stored newest-first. Give recent entries a material
    // advantage without overpowering exact or whole-word matches.
    match recency_rank {
        0..=4 => 180 - (recency_rank as i64 * 20),
        5..=14 => 90 - ((recency_rank as i64 - 5) * 6),
        _ => 0,
    }
}
