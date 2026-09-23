//! Multi-word place gazetteer (cities, regions, countries) as one Aho-Corasick
//! automaton over all inflected forms. Built once, scanned once per text.

use crate::dict::{Dictionaries, PlaceKind};
use crate::error::{CoreError, Result};
use crate::inflect::place_forms;
use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceHit {
    /// Offsets in the folded text.
    pub start: usize,
    pub end: usize,
    pub kind: PlaceKind,
}

pub struct Gazetteer {
    ac: AhoCorasick,
    kinds: Vec<PlaceKind>,
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-'
}

impl Gazetteer {
    pub fn build(dict: &Dictionaries) -> Result<Self> {
        let mut patterns: Vec<String> = Vec::new();
        let mut kinds = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for entry in &dict.places {
            for form in place_forms(&entry.name) {
                if form.chars().count() < 2 || !seen.insert(form.clone()) {
                    continue;
                }
                patterns.push(form);
                kinds.push(entry.kind);
            }
        }
        let ac = AhoCorasickBuilder::new()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&patterns)
            .map_err(|e| CoreError::Dictionary(format!("gazetteer build failed: {e}")))?;
        Ok(Self { ac, kinds })
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    /// Whole-word matches in the folded text, leftmost-longest, non-overlapping.
    pub fn find(&self, lower: &str) -> Vec<PlaceHit> {
        let mut hits = Vec::new();
        for m in self.ac.find_iter(lower) {
            let before_ok = lower[..m.start()]
                .chars()
                .next_back()
                .map(|c| !is_name_char(c))
                .unwrap_or(true);
            let after_ok = lower[m.end()..]
                .chars()
                .next()
                .map(|c| !is_name_char(c))
                .unwrap_or(true);
            if before_ok && after_ok {
                hits.push(PlaceHit {
                    start: m.start(),
                    end: m.end(),
                    kind: self.kinds[m.pattern().as_usize()],
                });
            }
        }
        hits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_inflected_places_as_whole_words() {
        let dict = Dictionaries::builtin();
        let g = Gazetteer::build(&dict).unwrap();
        let text = "родился в москве, живет в нижнем новгороде, гражданство россии, подмосковье";
        let hits = g.find(text);
        let texts: Vec<(&str, PlaceKind)> = hits
            .iter()
            .map(|h| (&text[h.start..h.end], h.kind))
            .collect();
        assert!(texts.contains(&("москве", PlaceKind::City)));
        assert!(texts.contains(&("нижнем новгороде", PlaceKind::City)));
        assert!(texts.contains(&("россии", PlaceKind::Country)));
        assert!(!texts.iter().any(|(t, _)| *t == "подмосковье"));
    }
}
