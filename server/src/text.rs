use std::collections::HashSet;

pub(crate) fn tokenize_query(input: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokenize_search_terms(input)
        .into_iter()
        .filter(|token| seen.insert(token.clone()))
        .collect()
}

pub(crate) fn tokenize_search_terms(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for piece in input.split(|character: char| {
        !character.is_alphanumeric() && character != '_' && character != '-'
    }) {
        push_token_variants(piece, &mut tokens);
    }
    tokens
}

fn push_token_variants(piece: &str, tokens: &mut Vec<String>) {
    if piece.is_empty() {
        return;
    }

    let normalized = piece.trim().to_lowercase();
    if normalized.len() >= 2 {
        tokens.push(normalized.clone());
    }

    for part in normalized.split(['_', '-']) {
        if part.len() >= 2 && part != normalized {
            tokens.push(part.to_string());
        }
    }

    let camel_parts = split_camel_case(piece);
    if camel_parts.len() > 1 {
        for part in camel_parts {
            if part.len() >= 2 {
                tokens.push(part.to_lowercase());
            }
        }
    }
}

fn split_camel_case(input: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let chars = input.char_indices().collect::<Vec<_>>();
    for window in chars.windows(2) {
        let (_, current) = window[0];
        let (next_index, next) = window[1];
        if current.is_ascii_lowercase() && next.is_ascii_uppercase() {
            if start < next_index {
                parts.push(&input[start..next_index]);
            }
            start = next_index;
        }
    }
    if start < input.len() {
        parts.push(&input[start..]);
    }
    parts
}

#[cfg(test)]
mod tests;
