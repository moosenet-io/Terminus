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

/// SUITE-STT: one number-word classification, used by [`normalize_spoken_numbers`].
enum NumTok {
    /// A units word (`zero`..`nineteen`) → its literal value.
    Unit(u64),
    /// A tens word (`twenty`..`ninety`) → its literal value.
    Ten(u64),
    /// The `hundred` magnitude.
    Hundred,
    /// The `thousand` magnitude.
    Thousand,
}

/// SUITE-STT: grammar state for the in-progress cardinal run folded by
/// [`normalize_spoken_numbers`]. Tracks WHAT was last consumed so an illegal
/// adjacency (e.g. a second bare unit word) breaks the run instead of silently
/// summing unrelated tokens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NumState {
    /// No active run.
    Idle,
    /// A bare units/teens word 0-19 (not following a tens word).
    Unit,
    /// A tens word (20..90), awaiting an optional trailing unit (1-9).
    TenWord,
    /// A unit that legally followed a tens word ("twenty three").
    TenUnit,
    /// Just consumed `hundred`.
    Hundred,
    /// A unit following `hundred` ("one hundred five").
    PostHundredUnit,
    /// A tens word following `hundred` ("one hundred twenty").
    PostHundredTen,
    /// A unit following a post-hundred tens word ("one hundred twenty three").
    PostHundredTenUnit,
    /// Just consumed `thousand`; a fresh sub-thousand group may begin.
    Thousand,
}

/// Classify a single lowercased, punctuation-trimmed token as a cardinal
/// number-word, or `None` if it isn't one. Cardinals only (ordinals like
/// `third` are intentionally NOT handled — see [`normalize_spoken_numbers`]).
fn classify_number_word(w: &str) -> Option<NumTok> {
    let v = match w {
        "zero" => NumTok::Unit(0),
        "one" => NumTok::Unit(1),
        "two" => NumTok::Unit(2),
        "three" => NumTok::Unit(3),
        "four" => NumTok::Unit(4),
        "five" => NumTok::Unit(5),
        "six" => NumTok::Unit(6),
        "seven" => NumTok::Unit(7),
        "eight" => NumTok::Unit(8),
        "nine" => NumTok::Unit(9),
        "ten" => NumTok::Unit(10),
        "eleven" => NumTok::Unit(11),
        "twelve" => NumTok::Unit(12),
        "thirteen" => NumTok::Unit(13),
        "fourteen" => NumTok::Unit(14),
        "fifteen" => NumTok::Unit(15),
        "sixteen" => NumTok::Unit(16),
        "seventeen" => NumTok::Unit(17),
        "eighteen" => NumTok::Unit(18),
        "nineteen" => NumTok::Unit(19),
        "twenty" => NumTok::Ten(20),
        "thirty" => NumTok::Ten(30),
        "forty" => NumTok::Ten(40),
        "fifty" => NumTok::Ten(50),
        "sixty" => NumTok::Ten(60),
        "seventy" => NumTok::Ten(70),
        "eighty" => NumTok::Ten(80),
        "ninety" => NumTok::Ten(90),
        "hundred" => NumTok::Hundred,
        "thousand" => NumTok::Thousand,
        _ => return None,
    };
    Some(v)
}

/// SUITE-STT: canonicalize spelled-out CARDINAL numbers to their digit form so
/// Word Error Rate is not inflated by a purely orthographic digit-vs-word
/// mismatch — an ASR model that emits `"23"` where the reference says
/// `"twenty three"` (or vice-versa) is transcribing correctly, and this
/// normalization (applied to BOTH sides before scoring, see
/// [`word_error_rate_normalized`]) makes them compare equal.
///
/// Lowercases, splits on whitespace, and trims leading/trailing punctuation
/// from each token (a hyphenated cardinal compound like `"twenty-three"` is
/// split on the hyphen and folds like `"twenty three"`). A run of adjacent
/// cardinal number-words is folded into a single digit token via the standard
/// units/tens/hundred/thousand accumulation — but ONLY while the words form a
/// grammatically valid cardinal: a bare sequence of separate unit words
/// (`"one two three"`) does NOT sum to `"6"`, it stays `"1 2 3"`, so distinct
/// transcripts never collapse to the same number. All accumulation is
/// saturating, so even a pathologically long run can never overflow/panic.
/// (`"one hundred twenty three"` → `"123"`); a bare `"and"`
/// *inside* such a run is skipped (`"one hundred and five"` → `"105"`). Tokens
/// that are already digits (`"2024"`, `"3.5"`) pass through unchanged, as do
/// all non-number words. Cardinals only — ORDINALS (`"third"`, `"twenty
/// third"`) are left as-is (the corpus baseline WER already accounts for the
/// handful of ordinal clips); scope-limiting to cardinals keeps the parser
/// small and correct rather than half-covering ordinal spelling variants.
pub fn normalize_spoken_numbers(text: &str) -> String {
    // Expand whitespace tokens, additionally splitting a HYPHENATED cardinal
    // compound ("twenty-three") into its parts so it folds like the spaced form.
    // A hyphenated token is only split when ALL its parts are number words, so
    // ordinary hyphenated prose ("state-of-the-art") is left untouched.
    let mut toks: Vec<(String, String)> = Vec::new(); // (normalized, raw-for-passthrough)
    for raw in text.split_whitespace() {
        let norm = raw
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase();
        if norm.is_empty() {
            continue;
        }
        if norm.contains('-') {
            let parts: Vec<String> = norm
                .split('-')
                .map(|p| p.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
                .filter(|p| !p.is_empty())
                .collect();
            if parts.len() >= 2 && parts.iter().all(|p| classify_number_word(p).is_some()) {
                for p in parts {
                    toks.push((p.clone(), p));
                }
                continue;
            }
        }
        toks.push((norm, raw.to_string()));
    }

    let mut out: Vec<String> = Vec::new();
    let mut state = NumState::Idle;
    let mut result: u64 = 0; // completed thousands portion
    let mut current: u64 = 0; // sub-thousand group being built

    let flush = |out: &mut Vec<String>, state: &mut NumState, result: &mut u64, current: &mut u64| {
        if *state != NumState::Idle {
            out.push(result.saturating_add(*current).to_string());
            *state = NumState::Idle;
            *result = 0;
            *current = 0;
        }
    };
    // Start a fresh run from a number word (all arithmetic saturating so a huge
    // run can never overflow/panic).
    let start = |cls: &NumTok, state: &mut NumState, result: &mut u64, current: &mut u64| match cls {
        NumTok::Unit(v) => {
            *current = *v;
            *state = NumState::Unit;
        }
        NumTok::Ten(v) => {
            *current = *v;
            *state = NumState::TenWord;
        }
        NumTok::Hundred => {
            *current = 100;
            *state = NumState::Hundred;
        }
        NumTok::Thousand => {
            *result = 1000;
            *state = NumState::Thousand;
        }
    };

    for (norm, raw) in &toks {
        // "and" only has meaning as a connector WITHIN a number run; drop it
        // there, keep it verbatim otherwise.
        if norm == "and" {
            if state != NumState::Idle {
                continue;
            }
            out.push(raw.clone());
            continue;
        }
        let Some(cls) = classify_number_word(norm) else {
            flush(&mut out, &mut state, &mut result, &mut current);
            out.push(raw.clone());
            continue;
        };
        // Does `cls` legally EXTEND the current partial cardinal per cardinal
        // grammar? A bare run of separate unit words ("one two three") does NOT
        // sum — each illegal adjacency flushes the run and restarts a fresh one,
        // so "one two three" stays "1 2 3", distinct from "six".
        use NumState::*;
        let extended = match (state, &cls) {
            // After a bare unit (0-19): only a magnitude extends it.
            (Unit, NumTok::Hundred) => {
                current = current.saturating_mul(100);
                Some(Hundred)
            }
            (Unit, NumTok::Thousand) => {
                result = result.saturating_add(current.saturating_mul(1000));
                current = 0;
                Some(Thousand)
            }
            // After a tens word ("twenty"): an optional trailing unit (1-9), or a
            // magnitude.
            (TenWord, NumTok::Unit(v)) if (1..=9).contains(v) => {
                current = current.saturating_add(*v);
                Some(TenUnit)
            }
            (TenWord, NumTok::Thousand) => {
                result = result.saturating_add(current.saturating_mul(1000));
                current = 0;
                Some(Thousand)
            }
            // After "twenty three": a magnitude.
            (TenUnit, NumTok::Hundred) => {
                current = current.saturating_mul(100);
                Some(Hundred)
            }
            (TenUnit, NumTok::Thousand) => {
                result = result.saturating_add(current.saturating_mul(1000));
                current = 0;
                Some(Thousand)
            }
            // After "hundred": a unit (0-19), a tens word, or thousand.
            (Hundred, NumTok::Unit(v)) => {
                current = current.saturating_add(*v);
                Some(PostHundredUnit)
            }
            (Hundred, NumTok::Ten(v)) => {
                current = current.saturating_add(*v);
                Some(PostHundredTen)
            }
            (Hundred, NumTok::Thousand) => {
                result = result.saturating_add(current.saturating_mul(1000));
                current = 0;
                Some(Thousand)
            }
            // After "one hundred five": only thousand.
            (PostHundredUnit, NumTok::Thousand) => {
                result = result.saturating_add(current.saturating_mul(1000));
                current = 0;
                Some(Thousand)
            }
            // After "one hundred twenty": an optional trailing unit, or thousand.
            (PostHundredTen, NumTok::Unit(v)) if (1..=9).contains(v) => {
                current = current.saturating_add(*v);
                Some(PostHundredTenUnit)
            }
            (PostHundredTen, NumTok::Thousand) => {
                result = result.saturating_add(current.saturating_mul(1000));
                current = 0;
                Some(Thousand)
            }
            // After "one hundred twenty three": only thousand.
            (PostHundredTenUnit, NumTok::Thousand) => {
                result = result.saturating_add(current.saturating_mul(1000));
                current = 0;
                Some(Thousand)
            }
            // After "thousand": a fresh sub-thousand group begins.
            (Thousand, NumTok::Unit(v)) => {
                current = current.saturating_add(*v);
                Some(Unit)
            }
            (Thousand, NumTok::Ten(v)) => {
                current = current.saturating_add(*v);
                Some(TenWord)
            }
            _ => None,
        };
        match extended {
            Some(ns) => state = ns,
            None => {
                // Illegal adjacency (or a fresh start from Idle): close the current
                // run and begin a new one from this number word.
                flush(&mut out, &mut state, &mut result, &mut current);
                start(&cls, &mut state, &mut result, &mut current);
            }
        }
    }
    flush(&mut out, &mut state, &mut result, &mut current);
    out.join(" ")
}

/// SUITE-STT: Word Error Rate after [`normalize_spoken_numbers`] is applied to
/// BOTH the hypothesis and the reference — the digit-normalized WER the STT
/// suite scores on (the corpus baseline of ~0.167 is measured with this
/// normalization). Otherwise identical to [`word_error_rate`].
pub fn word_error_rate_normalized(hypothesis: &str, reference: &str) -> f64 {
    word_error_rate(
        &normalize_spoken_numbers(hypothesis),
        &normalize_spoken_numbers(reference),
    )
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

    // ---- SUITE-STT: spelled-number normalization + normalized WER ----

    #[test]
    fn normalize_spoken_numbers_basic_cardinals() {
        assert_eq!(normalize_spoken_numbers("set a timer for ten minutes"), "set a timer for 10 minutes");
        assert_eq!(normalize_spoken_numbers("nine"), "9");
        assert_eq!(normalize_spoken_numbers("one hundred"), "100");
        assert_eq!(normalize_spoken_numbers("one hundred twenty three"), "123");
        assert_eq!(normalize_spoken_numbers("forty two"), "42");
        assert_eq!(normalize_spoken_numbers("two thousand twenty four"), "2024");
        // "and" as an in-number connector is dropped; elsewhere it survives.
        assert_eq!(normalize_spoken_numbers("one hundred and five apples and oranges"), "105 apples and oranges");
    }

    #[test]
    fn normalize_spoken_numbers_passthrough_and_punctuation() {
        // Already-digit and decimal tokens pass through untouched.
        assert_eq!(normalize_spoken_numbers("in 2024 it grew 3.5 percent"), "in 2024 it grew 3.5 percent");
        // Leading/trailing punctuation on a number word is trimmed before folding.
        assert_eq!(normalize_spoken_numbers("timer: ten."), "timer: 10");
    }

    /// b2fix finding 8: a bare run of separate unit words must NOT be summed —
    /// "one two three" stays distinct from "six", so different transcripts don't
    /// collapse to the same number.
    #[test]
    fn normalize_spoken_numbers_does_not_sum_bare_unit_runs() {
        assert_eq!(normalize_spoken_numbers("one two three"), "1 2 3");
        assert_ne!(
            normalize_spoken_numbers("one two three"),
            normalize_spoken_numbers("six"),
            "a bare unit run must not equal its sum"
        );
        // Real compound cardinals still fold correctly.
        assert_eq!(normalize_spoken_numbers("twenty three"), "23");
        assert_eq!(normalize_spoken_numbers("forty two"), "42");
        // Two tens words in a row do not merge either.
        assert_eq!(normalize_spoken_numbers("twenty thirty"), "20 30");
    }

    /// b2fix finding 8: hyphenated cardinals fold like their spaced form.
    #[test]
    fn normalize_spoken_numbers_handles_hyphenated_cardinals() {
        assert_eq!(normalize_spoken_numbers("twenty-three"), "23");
        assert_eq!(normalize_spoken_numbers("twenty-three"), normalize_spoken_numbers("23"));
        assert_eq!(normalize_spoken_numbers("one hundred twenty-three"), "123");
        // Non-number hyphenated prose is left intact.
        assert_eq!(normalize_spoken_numbers("a state-of-the-art model"), "a state-of-the-art model");
    }

    /// b2fix finding 8: a pathologically long run uses saturating arithmetic and
    /// must not overflow/panic.
    #[test]
    fn normalize_spoken_numbers_long_run_does_not_panic() {
        // 50k repetitions of a full "nine hundred ninety nine thousand" group —
        // exercises repeated saturating_add into `result` without panicking.
        let big = "nine hundred ninety nine thousand ".repeat(50_000);
        let out = normalize_spoken_numbers(&big);
        assert!(!out.is_empty());
        assert!(out.chars().all(|c| c.is_ascii_digit() || c == ' '));
    }

    #[test]
    fn word_error_rate_normalized_ignores_digit_word_mismatch() {
        // Pure orthographic digit-vs-word difference → 0.0 after normalization,
        // even though the raw WER is non-zero.
        let raw = word_error_rate("set a timer for 10 minutes", "set a timer for ten minutes");
        assert!(raw > 0.0, "raw WER should penalize the digit/word mismatch, got {raw}");
        let norm = word_error_rate_normalized("set a timer for 10 minutes", "set a timer for ten minutes");
        assert!(norm.abs() < 1e-9, "normalized WER should be 0.0, got {norm}");
    }
}
