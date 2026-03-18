use super::{tokenize_query, tokenize_search_terms};

#[test]
fn tokenize_query_deduplicates_code_like_terms() {
    let tokens = tokenize_query("CachePolicy cache_policy CachePolicy");
    assert!(tokens.contains(&"cachepolicy".to_string()));
    assert!(tokens.contains(&"cache".to_string()));
    assert!(tokens.contains(&"policy".to_string()));
    assert_eq!(
        tokens
            .iter()
            .filter(|token| token.as_str() == "cache")
            .count(),
        1
    );
}

#[test]
fn tokenize_search_terms_preserves_repeat_hits() {
    let tokens = tokenize_search_terms("cachePolicy cache policy");
    assert!(
        tokens
            .iter()
            .filter(|token| token.as_str() == "cache")
            .count()
            >= 2
    );
    assert!(
        tokens
            .iter()
            .filter(|token| token.as_str() == "policy")
            .count()
            >= 2
    );
}
