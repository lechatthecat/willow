//! Diagnostic text helpers for the type checker (extracted from `mod.rs`):
//! "did you mean" suggestions (edit distance).

pub(super) fn suggest_similar_name<'a>(
    target: &str,
    candidates: impl Iterator<Item = &'a String>,
) -> Option<String> {
    let max_distance = if target.len() <= 4 { 1 } else { 2 };
    candidates
        .map(|candidate| (levenshtein(target, candidate), candidate))
        .filter(|(distance, _)| *distance <= max_distance)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate.clone())
}

pub(super) fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars = b.chars().collect::<Vec<_>>();
    let mut prev = (0..=b_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0; b_chars.len() + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_chars.len()]
}
