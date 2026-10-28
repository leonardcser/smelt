pub mod history;
pub mod score;

pub use neo_frizbee::CaseMatching;

/// Whether `casing` makes matching against `query` case-sensitive. Smart
/// case turns sensitive once the query contains an uppercase character.
pub fn respects_case(casing: CaseMatching, query: &str) -> bool {
    match casing {
        CaseMatching::Ignore => false,
        CaseMatching::Smart => query.chars().any(char::is_uppercase),
        CaseMatching::Respect => true,
    }
}

fn matcher(query: &str, casing: CaseMatching) -> neo_frizbee::Matcher {
    let config = neo_frizbee::Config {
        casing,
        ..Default::default()
    };
    neo_frizbee::Matcher::new(query, &config)
}

/// Fuzzy-match `text` against `query`. `None` = no match; lower = better.
/// One-off pair scoring; for many candidates use [`fuzzy_rank`].
pub fn fuzzy_score(text: &str, query: &str, casing: CaseMatching) -> Option<u32> {
    let _perf = smelt_perf::perf::begin("fuzzy:score");
    if query.is_empty() {
        return Some(0);
    }
    let m = matcher(query, casing).match_one(text, 0)?;
    Some((u16::MAX - m.score) as u32)
}

/// Rank `haystacks` against `query`. Returns indices into `haystacks` in
/// best-first order, with non-matches dropped. Empty query → identity order.
pub fn fuzzy_rank<S: AsRef<str>>(query: &str, haystacks: &[S], casing: CaseMatching) -> Vec<usize> {
    let _perf = smelt_perf::perf::begin("fuzzy:rank");
    if query.is_empty() {
        return (0..haystacks.len()).collect();
    }
    matcher(query, casing)
        .match_list(haystacks)
        .into_iter()
        .map(|m| m.index as usize)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_prefix_wins() {
        let a = fuzzy_score("src/main.rs", "src", CaseMatching::Smart).unwrap();
        let b = fuzzy_score("crates/engine/src", "src", CaseMatching::Smart).unwrap();
        assert!(a < b, "prefix match should score better: {a} vs {b}");
    }

    #[test]
    fn shorter_path_wins() {
        let a = fuzzy_score("src/lib.rs", "src", CaseMatching::Smart).unwrap();
        let b = fuzzy_score("crates/engine/src/lib.rs", "src", CaseMatching::Smart).unwrap();
        assert!(a < b, "shorter match should score better: {a} vs {b}");
    }

    #[test]
    fn no_match() {
        assert!(fuzzy_score("hello", "xyz", CaseMatching::Smart).is_none());
    }

    #[test]
    fn empty_query_matches_all() {
        assert_eq!(fuzzy_score("anything", "", CaseMatching::Smart), Some(0));
    }

    #[test]
    fn boundary_match() {
        let a = fuzzy_score("Cargo.lock", "cl", CaseMatching::Smart).unwrap();
        let b = fuzzy_score("crates/engine/lib.rs", "cl", CaseMatching::Smart).unwrap();
        assert!(a < b, "shorter boundary match should win: {a} vs {b}");
    }

    #[test]
    fn smart_case_ignores_case_for_lowercase_queries() {
        let haystacks = ["README.md", "readme.rs", "src/main.rs"];
        assert_eq!(
            fuzzy_rank("readme", &haystacks, CaseMatching::Smart).len(),
            2
        );
    }

    #[test]
    fn smart_case_respects_case_once_query_has_uppercase() {
        let haystacks = ["README.md", "readme.rs", "src/main.rs"];
        assert_eq!(
            fuzzy_rank("README", &haystacks, CaseMatching::Smart),
            vec![0]
        );
        assert!(fuzzy_score("readme.rs", "README", CaseMatching::Smart).is_none());
    }

    #[test]
    fn ignore_case_matches_regardless_of_query_case() {
        assert!(fuzzy_score("readme.rs", "README", CaseMatching::Ignore).is_some());
    }

    #[test]
    fn respect_case_applies_to_lowercase_queries() {
        assert!(fuzzy_score("README.md", "readme", CaseMatching::Respect).is_none());
    }

    #[test]
    fn respects_case_follows_casing_mode() {
        assert!(!respects_case(CaseMatching::Smart, "readme"));
        assert!(respects_case(CaseMatching::Smart, "ReadMe"));
        assert!(!respects_case(CaseMatching::Ignore, "README"));
        assert!(respects_case(CaseMatching::Respect, "readme"));
    }
}
