use natural::phonetics::soundex;
use once_cell::sync::Lazy;
use regex::Regex;
use std::ops::Range;
use strsim::levenshtein;

/// Builds an n-gram string by cleaning and concatenating words
///
/// Strips punctuation from each word, lowercases, and joins without spaces.
/// This allows matching "Charge B" against "ChargeBee".
fn build_ngram(words: &[&str]) -> String {
    words
        .iter()
        .map(|w| build_match_key(w))
        .collect::<Vec<_>>()
        .concat()
}

fn build_match_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

struct CustomWordMatchKey {
    word_index: usize,
    key: String,
}

fn build_custom_word_match_keys(word: &str, word_index: usize) -> Vec<CustomWordMatchKey> {
    let primary_key = build_match_key(word);
    let mut keys = Vec::with_capacity(2);

    // The fallback matcher is intentionally limited to ASCII terms. Its
    // whitespace tokenization and Soundex scoring are not suitable for CJK
    // scripts. Unicode custom words remain available to models that accept
    // them as native decode prompts; they are simply skipped by this fallback.
    if is_supported_fuzzy_key(&primary_key) {
        keys.push(CustomWordMatchKey {
            word_index,
            key: primary_key.clone(),
        });
    }

    if word.contains('&') {
        let expanded_key = build_match_key(&word.replace('&', " and "));
        if is_supported_fuzzy_key(&expanded_key) && expanded_key != primary_key {
            keys.push(CustomWordMatchKey {
                word_index,
                key: expanded_key,
            });
        }
    }

    keys
}

fn is_supported_fuzzy_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric())
}

fn supports_soundex(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphabetic())
}

/// Finds the best matching custom word for a candidate string
///
/// Uses Levenshtein distance and Soundex phonetic matching to find
/// the best match above the given threshold.
///
/// # Arguments
/// * `candidate` - The cleaned/lowercased candidate string to match
/// * `custom_words` - Original custom words (for returning the replacement)
/// * `custom_word_match_keys` - Normalized custom-word keys for comparison
/// * `threshold` - Maximum similarity score to accept
///
/// # Returns
/// The best matching custom word and its score, if any match was found
fn find_best_match<'a>(
    candidate: &str,
    custom_words: &'a [String],
    custom_word_match_keys: &[CustomWordMatchKey],
    threshold: f64,
) -> Option<(&'a String, f64)> {
    if !is_supported_fuzzy_key(candidate) || candidate.chars().count() > 50 {
        return None;
    }

    let mut best_match: Option<&String> = None;
    let mut best_score = f64::MAX;

    for custom_word_key in custom_word_match_keys {
        // Skip if lengths are too different (optimization + prevents over-matching)
        // Use percentage-based check: max 25% length difference (prevents n-grams from
        // matching significantly shorter custom words, e.g., "openaigpt" vs "openai")
        let candidate_len = candidate.chars().count();
        let custom_word_len = custom_word_key.key.chars().count();
        let len_diff = candidate_len.abs_diff(custom_word_len) as f64;
        let max_len = candidate_len.max(custom_word_len) as f64;
        let max_allowed_diff = (max_len * 0.25).max(2.0); // At least 2 chars difference allowed
        if len_diff > max_allowed_diff {
            continue;
        }

        // Calculate Levenshtein distance (normalized by length)
        let levenshtein_dist = levenshtein(candidate, &custom_word_key.key);
        let levenshtein_score = if max_len > 0.0 {
            levenshtein_dist as f64 / max_len
        } else {
            1.0
        };

        // Soundex is an English/ASCII phonetic algorithm. Numeric terms can
        // still use edit distance, but must not receive a phonetic boost.
        let phonetic_match = supports_soundex(candidate)
            && supports_soundex(&custom_word_key.key)
            && soundex(candidate, &custom_word_key.key);

        // Combine scores: favor phonetic matches, but also consider string similarity
        let combined_score = if phonetic_match {
            levenshtein_score * 0.3 // Give significant boost to phonetic matches
        } else {
            levenshtein_score
        };

        // Accept if the score is good enough (configurable threshold)
        if combined_score < threshold && combined_score < best_score {
            best_match = Some(&custom_words[custom_word_key.word_index]);
            best_score = combined_score;
        }
    }

    best_match.map(|m| (m, best_score))
}

/// Applies custom word corrections to transcribed text using fuzzy matching
///
/// This function corrects words in the input text by finding the best matches
/// from a list of custom words using a combination of:
/// - Levenshtein distance for string similarity
/// - Soundex phonetic matching for pronunciation similarity
/// - N-gram matching for multi-word speech artifacts (e.g., "Charge B" -> "ChargeBee")
///
/// # Arguments
/// * `text` - The input text to correct
/// * `custom_words` - List of custom words to match against
/// * `threshold` - Maximum similarity score to accept (0.0 = exact match, 1.0 = any match)
///
/// # Returns
/// The corrected text with custom words applied
pub fn apply_custom_words(text: &str, custom_words: &[String], threshold: f64) -> String {
    if custom_words.is_empty() {
        return text.to_string();
    }

    // Pre-compute normalized comparison keys to avoid repeated allocations.
    let custom_word_match_keys: Vec<CustomWordMatchKey> = custom_words
        .iter()
        .enumerate()
        .flat_map(|(index, word)| build_custom_word_match_keys(word, index))
        .collect();

    let words: Vec<&str> = text.split_whitespace().collect();
    let mut result = Vec::new();
    let mut i = 0;

    while i < words.len() {
        let mut best_match: Option<(usize, &String, f64)> = None;

        // Consider n-grams up to three words and choose the closest match. A
        // longest-first match can consume a following ordinary word when both
        // candidates happen to share a Soundex code (for example,
        // "Charge B, che" matching "ChargeBee").
        for n in (1..=3).rev() {
            if i + n > words.len() {
                continue;
            }

            let ngram_words = &words[i..i + n];
            // Do not consume across a punctuation boundary. In
            // "Charge B, che", the comma closes the candidate at "B,".
            if ngram_words[..n.saturating_sub(1)]
                .iter()
                .any(|word| !extract_punctuation(word).1.is_empty())
            {
                continue;
            }
            let ngram = build_ngram(ngram_words);

            if let Some((replacement, score)) =
                find_best_match(&ngram, custom_words, &custom_word_match_keys, threshold)
            {
                let is_better = best_match
                    .as_ref()
                    .is_none_or(|(_, _, best_score)| score < *best_score);
                if is_better {
                    best_match = Some((n, replacement, score));
                }
            }
        }

        if let Some((n, replacement, _)) = best_match {
            let ngram_words = &words[i..i + n];
            // Extract punctuation from first and last words of the n-gram.
            let (prefix, _) = extract_punctuation(ngram_words[0]);
            let (_, suffix) = extract_punctuation(ngram_words[n - 1]);

            // Preserve case from first word.
            let corrected = preserve_case_pattern(ngram_words[0], replacement);

            result.push(format!("{}{}{}", prefix, corrected, suffix));
            i += n;
        } else {
            result.push(words[i].to_string());
            i += 1;
        }
    }

    result.join(" ")
}

/// Preserves the case pattern of the original word when applying a replacement
fn preserve_case_pattern(original: &str, replacement: &str) -> String {
    if original.chars().all(|c| c.is_uppercase()) {
        replacement.to_uppercase()
    } else if original.chars().next().is_some_and(|c| c.is_uppercase()) {
        let mut chars: Vec<char> = replacement.chars().collect();
        if let Some(first_char) = chars.get_mut(0) {
            *first_char = first_char.to_uppercase().next().unwrap_or(*first_char);
        }
        chars.into_iter().collect()
    } else {
        replacement.to_string()
    }
}

/// Extracts punctuation prefix and suffix from a word
fn extract_punctuation(word: &str) -> (&str, &str) {
    // String slices use byte offsets. Derive both boundaries from char_indices
    // so multibyte punctuation such as `。` and `「」` can never be split.
    let prefix_end = word
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, _)| index)
        .unwrap_or(word.len());
    let suffix_start = word
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(index, c)| index + c.len_utf8())
        .unwrap_or(0);

    let prefix = if prefix_end > 0 {
        &word[..prefix_end]
    } else {
        ""
    };

    let suffix = if suffix_start < word.len() {
        &word[suffix_start..]
    } else {
        ""
    };

    (prefix, suffix)
}

/// Returns filler words appropriate for the given language code.
///
/// Some sounds are meaningful words in other languages (for example,
/// Portuguese "um" = "a/an" and Russian "и" = "and"), so ordinary standalone
/// tokens are only removed when the transcription language makes that safe.
fn get_filler_words_for_language(lang: &str) -> &'static [&'static str] {
    let base_lang = lang.split(&['-', '_'][..]).next().unwrap_or(lang);

    match base_lang {
        "en" => &[
            "uh", "um", "uhm", "erm", "err", "er", "ah", "eh", "hmm", "hm", "mmm", "mm", "mh",
            "mhm",
        ],
        "es" => &["eh", "ehm", "em", "mmm", "hmm", "hm"],
        "pt" => &["ahm", "ahn", "hmm", "mmm", "hm"],
        "fr" => &["euh", "heu", "hmm", "hm", "mmm"],
        "de" => &["äh", "ähm", "hmm", "hm", "mmm"],
        "it" => &["eh", "ehm", "hmm", "mmm", "hm"],
        "cs" => &["eh", "ehm", "hmm", "mmm", "hm"],
        "pl" => &["yyy", "eee", "hmm", "mmm", "hm"],
        "tr" => &["hmm", "mmm", "hm"],
        "ru" => &["эм", "э", "хм", "гм", "кхм", "мгм", "мм", "hmm", "mmm"],
        "uk" => &["ем", "е", "хм", "гм", "кхм", "мгм", "мм", "hmm", "mmm"],
        "ar" => &["hmm", "mmm"],
        "ja" => &["hmm", "mmm"],
        "ko" => &["hmm", "mmm"],
        "vi" => &["hmm", "mmm", "hm"],
        "zh" => &["hmm", "mmm"],
        // Conservative fallback for auto-detect and unknown languages. Avoid
        // ambiguous standalone words such as English "um"/"er", Portuguese
        // "um", and Russian "а"/"и". Repeated sound sequences are handled
        // separately below and are safe across languages.
        _ => &["uh", "uhm", "uhh", "uhhh", "hmm", "mmm"],
    }
}

/// Resolve `auto` well enough for safe, language-specific filler removal.
///
/// Whisper-family batch transcription supplies its detected language to the
/// caller. Streaming and some ONNX engines do not, so use the output script as
/// a fallback. Russian and Ukrainian hesitation sounds share the forms handled
/// here; choosing `ru` for Cyrillic is therefore sufficient and avoids the old
/// `auto` behavior that retained every standalone Cyrillic filler.
fn resolve_filler_language<'a>(lang: &'a str, text: &str) -> &'a str {
    let base_lang = lang.split(&['-', '_'][..]).next().unwrap_or(lang);
    if !matches!(base_lang, "" | "auto" | "unknown" | "und") {
        return base_lang;
    }

    let has_cyrillic = text
        .chars()
        .any(|character| matches!(character as u32, 0x0400..=0x052f));
    if has_cyrillic {
        "ru"
    } else {
        base_lang
    }
}

static FILLER_TOKEN_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?iu)\b\p{L}+(?:[-‐‑‒–—]\p{L}+)*\b").unwrap());

fn canonical_sound_form(text: &str) -> String {
    let mut canonical = String::new();
    let mut previous = None;

    for character in text.chars().filter(|character| character.is_alphabetic()) {
        for lowercase in character.to_lowercase() {
            if previous != Some(lowercase) {
                canonical.push(lowercase);
                previous = Some(lowercase);
            }
        }
    }

    canonical
}

fn has_hesitation_rendering(text: &str) -> bool {
    let mut previous = None;
    let mut repeated_letter = false;
    let mut has_separator = false;

    for character in text.chars() {
        if matches!(character, '-' | '‐' | '‑' | '‒' | '–' | '—') {
            has_separator = true;
            previous = None;
            continue;
        }
        if !character.is_alphabetic() {
            continue;
        }

        let lowercase = character.to_lowercase().next().unwrap_or(character);
        if previous == Some(lowercase) {
            repeated_letter = true;
        }
        previous = Some(lowercase);
    }

    repeated_letter || has_separator
}

fn is_generated_hesitation_variant(text: &str, lang: &str) -> bool {
    if !has_hesitation_rendering(text) {
        return false;
    }

    let canonical = canonical_sound_form(text);
    let canonical_length = canonical.chars().count();
    if canonical.is_empty() || canonical_length > 3 {
        return false;
    }

    match lang {
        "ru" | "uk" => {
            let mut vowel_count = 0;
            let valid = canonical.chars().all(|character| {
                if matches!(
                    character,
                    'а' | 'э' | 'е' | 'ё' | 'ы' | 'и' | 'о' | 'у' | 'я'
                ) {
                    vowel_count += 1;
                    true
                } else {
                    matches!(character, 'м' | 'х' | 'г' | 'к')
                }
            });
            let starts_with_vowel = canonical.chars().next().is_some_and(|character| {
                matches!(
                    character,
                    'а' | 'э' | 'е' | 'ё' | 'ы' | 'и' | 'о' | 'у' | 'я'
                )
            });
            valid
                && ((starts_with_vowel && vowel_count == 1)
                    || canonical
                        .chars()
                        .all(|character| matches!(character, 'м' | 'х' | 'г')))
        }
        _ => {
            let mut vowel_count = 0;
            let valid = canonical.chars().all(|character| {
                if matches!(character, 'a' | 'e' | 'i' | 'o' | 'u' | 'ä' | 'y') {
                    vowel_count += 1;
                    true
                } else {
                    matches!(character, 'h' | 'm' | 'r' | 'n')
                }
            });
            let starts_with_vowel = canonical.chars().next().is_some_and(|character| {
                matches!(character, 'a' | 'e' | 'i' | 'o' | 'u' | 'ä' | 'y')
            });
            valid
                && ((starts_with_vowel && vowel_count == 1)
                    || canonical
                        .chars()
                        .all(|character| matches!(character, 'h' | 'm')))
        }
    }
}

fn collect_builtin_filler_matches(ranges: &mut Vec<Range<usize>>, text: &str, lang: &str) {
    let fillers = get_filler_words_for_language(lang);
    let canonical_fillers: Vec<String> = fillers
        .iter()
        .map(|filler| canonical_sound_form(filler))
        .collect();

    ranges.extend(FILLER_TOKEN_PATTERN.find_iter(text).filter_map(|matched| {
        let token = matched.as_str();
        if is_uppercase_acronym(token) {
            return None;
        }

        let canonical = canonical_sound_form(token);
        let lowercase = token.to_lowercase();
        let is_exact_filler = fillers.iter().any(|filler| lowercase == *filler);
        let is_rendered_filler =
            has_hesitation_rendering(token) && canonical_fillers.contains(&canonical);
        (is_exact_filler || is_rendered_filler || is_generated_hesitation_variant(token, lang))
            .then(|| matched.range())
    }));
}

static REPEATED_FILLER_SOUND_PATTERN: Lazy<Regex> = Lazy::new(|| {
    // Three or more filler syllables separated by whitespace or any common
    // hyphen/dash. This covers "uh uh uh", "um-um-um", and Russian
    // "а-а-а"/"э-э-э"/"и-и-и" without treating a single "а" or "и" as filler.
    // Each branch repeats the same sound family. A shared alternation at every
    // position would also match meaningful mixed tokens such as Russian "а и а".
    let separator = r"(?:[ \t]*[-‐‑‒–—][ \t]*|[ \t]+)";
    let repeated_sounds = [
        r"a+h+", r"u+h+", r"u+m+", r"e+r+", r"а+", r"э+", r"и+", r"эм+",
    ]
    .map(|sound| format!(r"{sound}(?:{separator}{sound}){{2,}}"))
    .join("|");
    Regex::new(&format!(r"(?iu)\b(?:{repeated_sounds})\b")).unwrap()
});

static ELONGATED_FILLER_SOUND_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?iu)\b(?:a{3,}h*|ah{2,}|u{2,}h+|uh{2,}|u{2,}m+|um{2,}|e{2,}r+|er{2,}|а{3,}|э{2,}|и{3,}|м{3,}|эм{2,})\b",
    )
    .unwrap()
});

static MULTI_HORIZONTAL_SPACE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[^\S\r\n]{2,}").unwrap());

fn is_uppercase_acronym(text: &str) -> bool {
    let letters: Vec<char> = text.chars().collect();
    let looks_like_acronym = letters.len() >= 2
        && letters.iter().all(|character| character.is_alphabetic())
        && letters.iter().all(|character| character.is_uppercase());

    // Retain the established cleanup for these unambiguous filler renderings;
    // other all-uppercase tokens are treated conservatively as abbreviations.
    looks_like_acronym
        && !matches!(
            text,
            "UH" | "UHM" | "ЭМ" | "ХМ" | "ММ" | "КХМ" | "МГМ" | "ЕМ"
        )
}

fn collect_removal_matches(
    ranges: &mut Vec<Range<usize>>,
    text: &str,
    pattern: &Regex,
    preserve_uppercase_acronyms: bool,
) {
    ranges.extend(pattern.find_iter(text).filter_map(|matched| {
        (!preserve_uppercase_acronyms || !is_uppercase_acronym(matched.as_str()))
            .then(|| matched.range())
    }));
}

fn is_soft_punctuation(character: char) -> bool {
    matches!(character, ',' | '，' | ';' | '；' | ':' | '：')
}

fn is_sentence_punctuation(character: char) -> bool {
    matches!(character, '.' | '!' | '?' | '。' | '！' | '？')
}

fn trim_horizontal_start(text: &str) -> &str {
    text.trim_start_matches(|character| matches!(character, ' ' | '\t'))
}

fn trim_horizontal_end(text: &str) -> &str {
    text.trim_end_matches(|character| matches!(character, ' ' | '\t'))
}

/// Removes punctuation and spacing made redundant at one concrete filler span.
///
/// Text away from the boundary is copied verbatim, so meaningful constructs such
/// as `std::io` and leading punctuation are never normalized as a side effect.
fn clean_removed_boundary(text: &mut String, boundary: usize) {
    let original_left = &text[..boundary];
    let original_right = &text[boundary..];
    let mut left = trim_horizontal_end(original_left);
    let mut right = trim_horizontal_start(original_right);
    let had_horizontal_gap =
        left.len() != original_left.len() || right.len() != original_right.len();

    let mut removed_right_soft_punctuation = false;
    while let Some(character) = right.chars().next() {
        if !is_soft_punctuation(character) {
            break;
        }
        right = trim_horizontal_start(&right[character.len_utf8()..]);
        removed_right_soft_punctuation = true;
    }

    if left.is_empty() {
        *text = right.to_string();
        return;
    }

    if right.is_empty() {
        if let Some((index, character)) = left.char_indices().next_back() {
            if is_soft_punctuation(character) {
                left = trim_horizontal_end(&left[..index]);
            }
        }
        *text = left.to_string();
        return;
    }

    if right.chars().next().is_some_and(is_sentence_punctuation) {
        if let Some((index, character)) = left.char_indices().next_back() {
            if is_soft_punctuation(character) {
                left = trim_horizontal_end(&left[..index]);
            }
        }
    }

    // ASR commonly surrounds a hesitation with two commas:
    // "I was, um, thinking". Once the filler and its trailing comma are gone,
    // the leading comma is redundant before a lowercase continuation. Retain
    // it before an uppercase token ("Well, um, I think") where it may still
    // represent a real clause/discourse boundary.
    if removed_right_soft_punctuation && right.chars().next().is_some_and(char::is_lowercase) {
        if let Some((index, character)) = left.char_indices().next_back() {
            if is_soft_punctuation(character) {
                left = trim_horizontal_end(&left[..index]);
            }
        }
    }

    let right_starts_with_sentence_punctuation =
        right.chars().next().is_some_and(is_sentence_punctuation);
    let boundary_needs_space = (had_horizontal_gap || removed_right_soft_punctuation)
        && !right_starts_with_sentence_punctuation
        && !left.ends_with('\r')
        && !left.ends_with('\n')
        && !right.starts_with('\r')
        && !right.starts_with('\n');

    let mut cleaned =
        String::with_capacity(left.len() + right.len() + usize::from(boundary_needs_space));
    cleaned.push_str(left);
    if boundary_needs_space {
        cleaned.push(' ');
    }
    cleaned.push_str(right);
    *text = cleaned;
}

fn remove_ranges_with_boundary_cleanup(text: &str, mut ranges: Vec<Range<usize>>) -> String {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut() {
            if range.start <= previous.end {
                previous.end = previous.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }

    let mut filtered = text.to_string();
    for range in merged.into_iter().rev() {
        filtered.replace_range(range.clone(), "");
        clean_removed_boundary(&mut filtered, range.start);
    }
    filtered
}

/// Filters transcription output by removing filler sounds.
///
/// This function cleans up raw transcription text by:
/// 1. Removing unambiguous repeated or elongated filler sounds across languages
/// 2. Removing standalone filler tokens that are safe for the selected language
/// 3. Cleaning up only whitespace and punctuation made redundant by removal
///
/// # Arguments
/// * `text` - The raw transcription text to filter
/// * `lang` - The transcription language code (e.g., "en", "pt-BR") used to select fillers
/// * `custom_filler_words` - Optional user-provided filler word list. `Some(vec)` overrides
///   language defaults; `None` uses language defaults.
///
/// # Returns
/// The filtered text with filler sounds removed and ordinary words preserved
pub fn filter_transcription_output(
    text: &str,
    lang: &str,
    custom_filler_words: &Option<Vec<String>>,
) -> String {
    let lang = resolve_filler_language(lang, text);
    let mut removal_ranges = Vec::new();
    collect_removal_matches(
        &mut removal_ranges,
        text,
        &REPEATED_FILLER_SOUND_PATTERN,
        true,
    );
    collect_removal_matches(
        &mut removal_ranges,
        text,
        &ELONGATED_FILLER_SOUND_PATTERN,
        true,
    );

    match custom_filler_words {
        Some(words) => {
            for pattern in words.iter().filter_map(|word| {
                let word = word.trim();
                (!word.is_empty())
                    .then(|| Regex::new(&format!(r"(?iu)\b{}\b", regex::escape(word))).ok())
                    .flatten()
            }) {
                collect_removal_matches(&mut removal_ranges, text, &pattern, false);
            }
        }
        None => {
            collect_builtin_filler_matches(&mut removal_ranges, text, lang);
        }
    }

    let mut filtered = remove_ranges_with_boundary_cleanup(text, removal_ranges);
    filtered = MULTI_HORIZONTAL_SPACE_PATTERN
        .replace_all(&filtered, " ")
        .to_string();

    // Trim leading/trailing whitespace
    filtered.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_apply_custom_words_exact_match() {
        let text = "hello world";
        let custom_words = vec!["Hello".to_string(), "World".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_apply_custom_words_fuzzy_match() {
        let text = "helo wrold";
        let custom_words = vec!["hello".to_string(), "world".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_preserve_case_pattern() {
        assert_eq!(preserve_case_pattern("HELLO", "world"), "WORLD");
        assert_eq!(preserve_case_pattern("Hello", "world"), "World");
        assert_eq!(preserve_case_pattern("hello", "WORLD"), "WORLD");
    }

    #[test]
    fn test_extract_punctuation() {
        assert_eq!(extract_punctuation("hello"), ("", ""));
        assert_eq!(extract_punctuation("!hello?"), ("!", "?"));
        assert_eq!(extract_punctuation("...hello..."), ("...", "..."));
    }

    #[test]
    fn test_extract_punctuation_uses_unicode_boundaries() {
        assert_eq!(extract_punctuation("你好。"), ("", "。"));
        assert_eq!(extract_punctuation("「你好」"), ("「", "」"));
        assert_eq!(extract_punctuation("你好！"), ("", "！"));
    }

    #[test]
    fn test_empty_custom_words() {
        let text = "hello world";
        let custom_words = vec![];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_filter_filler_words() {
        let text = "So uhm I was thinking uh about this";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "So I was thinking about this");
    }

    #[test]
    fn test_filter_filler_words_case_insensitive() {
        let text = "UHM this is UH a test";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "this is a test");
    }

    #[test]
    fn test_filter_filler_words_with_punctuation() {
        let text = "Well, uhm, I think, uh. that's right";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Well, I think. that's right");
    }

    #[test]
    fn test_filter_cleans_whitespace() {
        let text = "Hello    world   test";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Hello world test");
    }

    #[test]
    fn test_filter_trims() {
        let text = "  Hello world  ";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_filter_combined() {
        let text = "  Uhm, so I was, uh, thinking about this  ";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "so I was thinking about this");
    }

    #[test]
    fn test_filter_preserves_valid_text() {
        let text = "This is a completely normal sentence.";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "This is a completely normal sentence.");
    }

    #[test]
    fn test_filter_removes_repeated_filler_sounds() {
        let text = "Well uh uh uh I think um-um-um this works";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "Well I think this works");
    }

    #[test]
    fn test_filter_removes_russian_repeated_filler_sounds() {
        let text = "Это а-а-а пример э э э текста и-и-и всё";
        let result = filter_transcription_output(text, "auto", &None);
        assert_eq!(result, "Это пример текста всё");
    }

    #[test]
    fn test_filter_removes_elongated_filler_sounds() {
        let text = "So uhhh this is эээ useful";
        let result = filter_transcription_output(text, "auto", &None);
        assert_eq!(result, "So this is useful");
    }

    #[test]
    fn test_filter_auto_detects_cyrillic_fillers() {
        let text = "ЭМ, это, э, должно работать, хм, всегда";
        let result = filter_transcription_output(text, "auto", &None);
        assert_eq!(result, "это должно работать всегда");
    }

    #[test]
    fn test_filter_canonicalizes_unseen_filler_renderings() {
        let text = "Эээээмммм, это ааааххх, пример ыыы и э-э-м";
        let result = filter_transcription_output(text, "auto", &None);
        assert_eq!(result, "это пример и");
    }

    #[test]
    fn test_filter_canonicalizes_unseen_english_renderings() {
        let text = "Uhhhhhhhh, this is ummmmmmmmm, still eeeerrrr fine";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "this is still fine");
    }

    #[test]
    fn test_filter_preserves_elongated_ordinary_words() {
        let text = "Это длиииинный текст and heyyy noooo";
        let result = filter_transcription_output(text, "auto", &None);
        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_does_not_reduce_canonical_mmm_to_letter_m() {
        let text = "Use m as the variable";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_preserves_meaningful_repetition() {
        let text = "I I I really mean very very very important";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_preserves_russian_conjunctions() {
        let text = "А я и ты";
        let result = filter_transcription_output(text, "ru", &None);
        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_preserves_mixed_russian_tokens() {
        let text = "а и а";
        let result = filter_transcription_output(text, "ru", &None);
        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_preserves_uppercase_er_abbreviation() {
        let text = "er I went to the ER";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "I went to the ER");
    }

    #[test]
    fn test_filter_preserves_uppercase_acronyms() {
        let text = "AAA battery reported ERR";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_does_not_repair_unrelated_punctuation() {
        let text = ":root uses std::io";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, text);
    }

    #[test]
    fn test_filter_repairs_only_the_removed_filler_boundary() {
        let text = "um, std::io handles ERR";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "std::io handles ERR");
    }

    #[test]
    fn test_filter_english_removes_um() {
        let text = "um I think um this is good";
        let result = filter_transcription_output(text, "en", &None);
        assert_eq!(result, "I think this is good");
    }

    #[test]
    fn test_filter_portuguese_preserves_um() {
        // "um" means "a/an" in Portuguese
        let text = "um gato bonito";
        let result = filter_transcription_output(text, "pt", &None);
        assert_eq!(result, "um gato bonito");
    }

    #[test]
    fn test_filter_spanish_preserves_ha() {
        // "ha" means "has" in Spanish
        let text = "ha sido un buen día";
        let result = filter_transcription_output(text, "es", &None);
        assert_eq!(result, "ha sido un buen día");
    }

    #[test]
    fn test_filter_language_code_with_region() {
        // "pt-BR" should normalize to "pt"
        let text = "um gato bonito";
        let result = filter_transcription_output(text, "pt-BR", &None);
        assert_eq!(result, "um gato bonito");
    }

    #[test]
    fn test_filter_custom_filler_words_override() {
        let custom = Some(vec!["okay".to_string(), "right".to_string()]);
        let text = "okay so I think right this works";
        let result = filter_transcription_output(text, "en", &custom);
        assert_eq!(result, "so I think this works");
    }

    #[test]
    fn test_filter_custom_filler_words_empty_disables() {
        let custom = Some(vec![]);
        let text = "So uhm I was thinking uh about this";
        let result = filter_transcription_output(text, "en", &custom);
        // No filler words removed since custom list is empty
        assert_eq!(result, "So uhm I was thinking uh about this");
    }

    #[test]
    fn test_filter_unknown_language_uses_fallback() {
        let text = "uh I think uhm this works";
        let result = filter_transcription_output(text, "xx", &None);
        assert_eq!(result, "I think this works");
    }

    #[test]
    fn test_filter_fallback_does_not_remove_um() {
        // Fallback (unknown language) should not remove "um" since it's a real word in some languages
        let text = "um I think this works";
        let result = filter_transcription_output(text, "xx", &None);
        assert_eq!(result, "um I think this works");
    }

    #[test]
    fn test_apply_custom_words_ngram_two_words() {
        let text = "il cui nome è Charge B, che permette";
        let custom_words = vec!["ChargeBee".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("ChargeBee,"), "unexpected result: {result}");
        assert!(!result.contains("Charge B"));
    }

    #[test]
    fn test_apply_custom_words_ngram_three_words() {
        let text = "use Chat G P T for this";
        let custom_words = vec!["ChatGPT".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("ChatGPT"));
    }

    #[test]
    fn test_apply_custom_words_prefers_longer_ngram() {
        let text = "Open AI GPT model";
        let custom_words = vec!["OpenAI".to_string(), "GPT".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "OpenAI GPT model");
    }

    #[test]
    fn test_apply_custom_words_ngram_preserves_case() {
        let text = "CHARGE B is great";
        let custom_words = vec!["ChargeBee".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("CHARGEBEE"));
    }

    #[test]
    fn test_apply_custom_words_ngram_with_spaces_in_custom() {
        // Custom word with space should also match against split words
        let text = "using Mac Book Pro";
        let custom_words = vec!["MacBook Pro".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert!(result.contains("MacBook"));
    }

    #[test]
    fn test_apply_custom_words_trailing_number_not_doubled() {
        // Verify that trailing non-alpha chars (like numbers) aren't double-counted
        // between build_ngram stripping them and extract_punctuation capturing them
        let text = "use GPT4 for this";
        let custom_words = vec!["GPT-4".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        // Should NOT produce "GPT-44" (double-counting the trailing 4)
        assert!(
            !result.contains("GPT-44"),
            "got double-counted result: {}",
            result
        );
    }

    #[test]
    fn test_apply_custom_words_matches_ampersand_word() {
        let text = "send it to RD for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_matches_spoken_ampersand_word() {
        let text = "send it to R and D for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_preserves_ampersand_word() {
        let text = "send it to R&D for review";
        let custom_words = vec!["R&D".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.18);
        assert_eq!(result, "send it to R&D for review");
    }

    #[test]
    fn test_apply_custom_words_handles_unicode_punctuation() {
        let text = "「Handee。」";
        let custom_words = vec!["Handy".to_string()];
        let result = apply_custom_words(text, &custom_words, 0.5);
        assert_eq!(result, "「Handy。」");
    }

    #[test]
    fn test_apply_custom_words_skips_cjk_fuzzy_matching() {
        let text = "你好。";
        let custom_words = vec!["你号".to_string()];
        let result = apply_custom_words(text, &custom_words, 1.0);
        assert_eq!(result, text);
    }
}
