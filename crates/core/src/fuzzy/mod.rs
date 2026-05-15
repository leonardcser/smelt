pub mod score;

/// neo_frizbee's SIMD load path over-reads up to 8 bytes past the haystack
/// pointer. The over-read is page-boundary-guarded but not allocator-
/// boundary-guarded, so ASan flags it as a heap-buffer-overflow on
/// haystacks under 8 bytes. Materializing the input into a `String` with
/// capacity ≥ 8 keeps the over-read inside the allocation.
///
/// TODO: drop this workaround once the upstream fix lands and
/// neo_frizbee rebases: https://github.com/saghen/frizbee/pull/78
const SIMD_MIN_CAP: usize = 8;

fn pad_short(s: &str) -> String {
    let mut padded = String::with_capacity(s.len().max(SIMD_MIN_CAP));
    padded.push_str(s);
    padded
}

/// Fuzzy-match `text` against `query`. `None` = no match; lower = better.
/// One-off pair scoring; for many candidates use [`fuzzy_rank`].
pub fn fuzzy_score(text: &str, query: &str) -> Option<u32> {
    let _perf = smelt_perf::perf::begin("fuzzy:score");
    if query.is_empty() {
        return Some(0);
    }
    let mut matcher = neo_frizbee::Matcher::new(query, &neo_frizbee::Config::default());
    let padded;
    let bytes = if text.len() < SIMD_MIN_CAP {
        padded = pad_short(text);
        padded.as_bytes()
    } else {
        text.as_bytes()
    };
    let m = matcher.smith_waterman_one(bytes, 0, true)?;
    Some((u16::MAX - m.score) as u32)
}

/// Rank `haystacks` against `query`. Returns indices into `haystacks` in
/// best-first order, with non-matches dropped. Empty query → identity order.
pub fn fuzzy_rank<S: AsRef<str>>(query: &str, haystacks: &[S]) -> Vec<usize> {
    let _perf = smelt_perf::perf::begin("fuzzy:rank");
    if query.is_empty() {
        return (0..haystacks.len()).collect();
    }
    if haystacks.iter().any(|s| s.as_ref().len() < SIMD_MIN_CAP) {
        let padded: Vec<String> = haystacks
            .iter()
            .map(|s| {
                let s = s.as_ref();
                if s.len() >= SIMD_MIN_CAP {
                    s.to_string()
                } else {
                    pad_short(s)
                }
            })
            .collect();
        return neo_frizbee::match_list(query, &padded, &neo_frizbee::Config::default())
            .into_iter()
            .map(|m| m.index as usize)
            .collect();
    }
    neo_frizbee::match_list(query, haystacks, &neo_frizbee::Config::default())
        .into_iter()
        .map(|m| m.index as usize)
        .collect()
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
    fn no_match() {
        assert!(fuzzy_score("hello", "xyz").is_none());
    }

    #[test]
    fn empty_query_matches_all() {
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn boundary_match() {
        let a = fuzzy_score("Cargo.lock", "cl").unwrap();
        let b = fuzzy_score("crates/engine/lib.rs", "cl").unwrap();
        assert!(a < b, "shorter boundary match should win: {a} vs {b}");
    }
}
