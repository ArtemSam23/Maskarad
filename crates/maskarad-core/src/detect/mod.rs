//! Detectors produce [`Candidate`]s; the resolver decides which of them are
//! personal data. Every detector works on the folded text and maps spans back
//! to the original.

pub mod address;
pub mod context;
pub mod dates;
pub mod gazetteer;
pub mod issuer;
pub mod pattern;
pub mod person;
pub mod place;
pub mod tokens;
pub mod validators;

use crate::dict::Dictionaries;
use crate::normalize::Normalized;
use crate::types::Candidate;
use gazetteer::PlaceHit;
use tokens::Word;

/// Everything a detector may look at. Built once per request.
pub struct DetectCtx<'a> {
    /// Original text.
    pub text: &'a str,
    /// Folded text with the offset map.
    pub norm: &'a Normalized,
    /// Words of the folded text.
    pub words: &'a [Word],
    pub dict: &'a Dictionaries,
    /// Gazetteer hits (cities, regions, countries) in the folded text.
    pub places: &'a [PlaceHit],
    /// Default context window in characters.
    pub window: usize,
}

pub trait Detector: Send + Sync {
    fn name(&self) -> &'static str;
    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>);
}

/// The match is not glued to other digits (`123` inside `41234`).
pub fn digit_boundary(lower: &str, start: usize, end: usize) -> bool {
    let before = lower[..start]
        .chars()
        .next_back()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false);
    let after = lower[end..]
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false);
    !before && !after
}
