//! Case folding with an offset map back to the original text.
//!
//! All detectors run on a folded copy (lower case, `ё` → `е`, typographic
//! dashes and non-breaking spaces unified) so that detection is independent
//! of letter case and typography by construction. Every span found in the
//! folded copy is mapped back to byte offsets in the original text.

use crate::types::Span;

#[derive(Clone, Debug)]
pub struct Normalized {
    /// Folded text.
    pub lower: String,
    /// `map[i]` = byte offset in the original text of the character that
    /// produced folded byte `i`; `map[lower.len()]` = original length.
    map: Vec<u32>,
    orig_len: usize,
}

impl Normalized {
    pub fn new(text: &str) -> Self {
        let mut lower = String::with_capacity(text.len());
        let mut map = Vec::with_capacity(text.len() + 1);
        for (offset, ch) in text.char_indices() {
            let before = lower.len();
            push_folded(ch, &mut lower);
            for _ in before..lower.len() {
                map.push(offset as u32);
            }
        }
        map.push(text.len() as u32);
        Self {
            lower,
            map,
            orig_len: text.len(),
        }
    }

    /// Original offset of the character containing folded byte `lower_offset`.
    pub fn to_orig_start(&self, lower_offset: usize) -> usize {
        self.map[lower_offset.min(self.lower.len())] as usize
    }

    /// Original offset corresponding to the exclusive end `lower_end`.
    pub fn to_orig_end(&self, lower_end: usize) -> usize {
        if lower_end >= self.lower.len() {
            return self.orig_len;
        }
        if lower_end == 0 {
            return 0;
        }
        let here = self.map[lower_end];
        if here != self.map[lower_end - 1] {
            return here as usize;
        }
        // Inside a multi-byte expansion of one original character: round up.
        let mut i = lower_end;
        while i < self.lower.len() && self.map[i] == here {
            i += 1;
        }
        self.map[i] as usize
    }

    pub fn to_orig_span(&self, start: usize, end: usize) -> Span {
        Span::new(self.to_orig_start(start), self.to_orig_end(end))
    }

    pub fn orig_len(&self) -> usize {
        self.orig_len
    }

    /// Folded offset of an original byte offset (binary search over the map).
    pub fn lower_offset(&self, orig: usize) -> usize {
        match self.map.binary_search(&(orig as u32)) {
            Ok(mut i) => {
                while i > 0 && self.map[i - 1] == orig as u32 {
                    i -= 1;
                }
                i
            }
            Err(i) => i.min(self.lower.len()),
        }
    }
}

fn push_folded(ch: char, out: &mut String) {
    match ch {
        'Ё' | 'ё' => out.push('е'),
        '\u{a0}' | '\u{2007}' | '\u{202f}' | '\t' => out.push(' '),
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => {
            out.push('-')
        }
        _ => {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_and_yo() {
        let n = Normalized::new("Пётр ИВАНОВ");
        assert_eq!(n.lower, "петр иванов");
    }

    #[test]
    fn maps_offsets_back() {
        let text = "Тел: +7 925 123–45–67"; // en dashes (3 bytes) fold to '-' (1 byte)
        let n = Normalized::new(text);
        let start = n.lower.find("+7").unwrap();
        let end = n.lower.len();
        let span = n.to_orig_span(start, end);
        assert_eq!(&text[span.start..span.end], "+7 925 123–45–67");
    }

    #[test]
    fn end_inside_expansion_rounds_up() {
        // 'İ' lowercases to two chars ("i̇"); an end offset inside must round up.
        let text = "aİb";
        let n = Normalized::new(text);
        let span = n.to_orig_span(0, 2);
        assert_eq!(&text[span.start..span.end], "aİ");
    }
}
