//! Dependency-free text-similarity primitives shared by the new-category
//! harnesses (`document_parsing`, `image_parsing`, `voice_transcription`).
//!
//! No crate is added for this (no `strsim`/`edit-distance` dependency): the
//! algorithms are small, standard, and easy to keep correct in-tree.

/// Classic Levenshtein (single-character insert/delete/substitute) edit
/// distance between two token/char sequences. Generic over any `PartialEq`
/// element so it can operate on `char`s (for `normalized_edit_similarity`) or
/// `&str` words (for [`word_error_rate`]).
pub fn levenshtein<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1) // deletion
                .min(cur[j - 1] + 1) // insertion
                .min(prev[j - 1] + cost); // substitution
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Normalized character-level edit similarity in `[0.0, 1.0]`: `1.0` means
/// identical strings, `0.0` means the edit distance is at least as long as the
/// longer string. Case-insensitive, whitespace-trimmed (this is a coarse
/// "close enough" signal, not a precise diff).
pub fn normalized_edit_similarity(a: &str, b: &str) -> f64 {
    let a_norm = a.trim().to_lowercase();
    let b_norm = b.trim().to_lowercase();
    let a_chars: Vec<char> = a_norm.chars().collect();
    let b_chars: Vec<char> = b_norm.chars().collect();
    let max_len = a_chars.len().max(b_chars.len());
    if max_len == 0 {
        return 1.0; // both empty: trivially identical
    }
    let dist = levenshtein(&a_chars, &b_chars);
    1.0 - (dist as f64 / max_len as f64).min(1.0)
}

/// Token-overlap (Jaccard) similarity in `[0.0, 1.0]` over lowercased,
/// whitespace-split words. Order-insensitive, so "a red solid square" and
/// "a solid red square" score identically — appropriate for caption-style
/// scoring where word order carries little signal.
pub fn token_jaccard(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    let a_set: HashSet<String> = a
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let b_set: HashSet<String> = b
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if a_set.is_empty() && b_set.is_empty() {
        return 1.0;
    }
    if a_set.is_empty() || b_set.is_empty() {
        return 0.0;
    }
    let intersection = a_set.intersection(&b_set).count();
    let union = a_set.union(&b_set).count();
    intersection as f64 / union as f64
}

/// Word Error Rate: `(substitutions + insertions + deletions) / len(reference words)`,
/// computed via word-level Levenshtein alignment between `hypothesis` and
/// `reference`. Standard ASR-quality metric — 0.0 is a perfect transcript,
/// values > 1.0 are possible (hypothesis much longer/garbled than reference).
/// Case-insensitive, whitespace-tokenized.
pub fn word_error_rate(hypothesis: &str, reference: &str) -> f64 {
    let ref_words: Vec<String> = reference
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    let hyp_words: Vec<String> = hypothesis
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    if ref_words.is_empty() {
        return if hyp_words.is_empty() { 0.0 } else { 1.0 };
    }
    let dist = levenshtein(&hyp_words, &ref_words);
    dist as f64 / ref_words.len() as f64
}

/// `1.0 - min(WER, 1.0)` — a `[0.0, 1.0]` "accuracy" convenience so callers
/// storing a `value` column don't need to remember WER is unbounded-above and
/// lower-is-better. Clamped at 0.0 (a WER > 1.0 still reads as "0% accurate",
/// not negative).
pub fn wer_to_accuracy(wer: f64) -> f64 {
    (1.0 - wer).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_identical_is_zero() {
        assert_eq!(levenshtein(&['a', 'b', 'c'], &['a', 'b', 'c']), 0);
    }

    #[test]
    fn levenshtein_totally_different_is_max_len() {
        assert_eq!(levenshtein(&['a', 'b'], &['x', 'y', 'z']), 3);
    }

    #[test]
    fn edit_similarity_identical_strings_is_one() {
        let s = normalized_edit_similarity("a solid red square", "a solid red square");
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {s}");
    }

    #[test]
    fn edit_similarity_garbled_string_is_low() {
        let s = normalized_edit_similarity("a solid red square", "zzz qqq xx nothing here");
        assert!(s < 0.3, "expected low similarity for garbled text, got {s}");
    }

    #[test]
    fn token_jaccard_identical_bag_is_one() {
        let s = token_jaccard("a solid red square", "solid red a square");
        assert!((s - 1.0).abs() < 1e-9, "expected 1.0, got {s}");
    }

    #[test]
    fn token_jaccard_disjoint_is_zero() {
        let s = token_jaccard("a solid red square", "nothing matches here at all");
        assert!(s < 1e-9, "expected 0.0, got {s}");
    }

    #[test]
    fn wer_perfect_transcript_is_zero() {
        let w = word_error_rate(
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox jumps over the lazy dog",
        );
        assert!(w.abs() < 1e-9, "expected WER 0.0, got {w}");
        assert!((wer_to_accuracy(w) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn wer_garbled_transcript_is_high() {
        let w = word_error_rate(
            "asdf qwer zxcv random nonsense words here",
            "the quick brown fox jumps over the lazy dog",
        );
        assert!(w > 0.7, "expected high WER for garbled transcript, got {w}");
        assert!(
            wer_to_accuracy(w) < 0.3,
            "expected low accuracy for garbled transcript, got {}",
            wer_to_accuracy(w)
        );
    }
}
