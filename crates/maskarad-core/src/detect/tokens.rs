//! Word tokenizer over the folded text.

use crate::normalize::Normalized;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordKind {
    Cyrillic,
    Latin,
    Digits,
    Mixed,
}

/// A maximal run of alphanumeric characters (hyphens and apostrophes allowed
/// inside), with offsets into the folded text and case information taken
/// from the original text.
#[derive(Clone, Copy, Debug)]
pub struct Word {
    pub start: usize,
    pub end: usize,
    pub kind: WordKind,
    /// First letter is upper case in the original text.
    pub capitalized: bool,
    /// Every letter is upper case in the original text (two or more letters).
    pub all_caps: bool,
}

impl Word {
    pub fn text<'a>(&self, lower: &'a str) -> &'a str {
        &lower[self.start..self.end]
    }
}

fn is_cyrillic(c: char) -> bool {
    matches!(c, 'а'..='я' | 'ё' | 'А'..='Я' | 'Ё' | 'і' | 'ї' | 'є' | 'ґ' | 'ў')
}

fn is_joiner(c: char) -> bool {
    matches!(c, '-' | '\'' | '’')
}

pub fn tokenize(text: &str, norm: &Normalized) -> Vec<Word> {
    let lower = norm.lower.as_str();
    let mut words = Vec::with_capacity(lower.len() / 6 + 1);
    let mut iter = lower.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if !c.is_alphanumeric() {
            continue;
        }
        let start = i;
        let mut end = i + c.len_utf8();
        let (mut cyr, mut lat, mut dig) =
            (is_cyrillic(c), c.is_ascii_alphabetic(), c.is_ascii_digit());
        loop {
            match iter.peek().copied() {
                Some((j, d)) if d.is_alphanumeric() => {
                    cyr |= is_cyrillic(d);
                    lat |= d.is_ascii_alphabetic();
                    dig |= d.is_ascii_digit();
                    end = j + d.len_utf8();
                    iter.next();
                }
                Some((_, d)) if is_joiner(d) => {
                    let mut look = iter.clone();
                    look.next();
                    match look.peek() {
                        Some((_, n)) if n.is_alphanumeric() => {
                            iter.next();
                        }
                        _ => break,
                    }
                }
                _ => break,
            }
        }
        let kind = match (cyr, lat, dig) {
            (true, false, false) => WordKind::Cyrillic,
            (false, true, false) => WordKind::Latin,
            (false, false, true) => WordKind::Digits,
            _ => WordKind::Mixed,
        };
        let span = norm.to_orig_span(start, end);
        let original = &text[span.start..span.end];
        let mut letters = original.chars().filter(|c| c.is_alphabetic());
        let capitalized = letters.next().map(|c| c.is_uppercase()).unwrap_or(false);
        let letter_count = original.chars().filter(|c| c.is_alphabetic()).count();
        let all_caps = letter_count >= 2
            && original
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(|c| c.is_uppercase());
        words.push(Word {
            start,
            end,
            kind,
            capitalized,
            all_caps,
        });
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_words_and_keeps_case_info() {
        let text = "Клиент Римский-Корсаков, IVAN, тел 8-925";
        let norm = Normalized::new(text);
        let words = tokenize(text, &norm);
        let texts: Vec<&str> = words.iter().map(|w| w.text(&norm.lower)).collect();
        assert_eq!(
            texts,
            vec!["клиент", "римский-корсаков", "ivan", "тел", "8-925"]
        );
        assert!(words[1].capitalized);
        assert!(words[2].all_caps);
        assert_eq!(words[2].kind, WordKind::Latin);
        assert_eq!(words[4].kind, WordKind::Digits);
    }
}
