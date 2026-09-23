//! Birth place and citizenship: a cue word followed by a place name.

use super::context::compile_alternation;
use super::gazetteer::PlaceHit;
use super::{DetectCtx, Detector};
use crate::dict::PlaceKind;
use crate::error::Result;
use crate::types::{pii, Candidate};
use regex::Regex;

const SETTLEMENT_MARKER: &str = r"^(?:г\.|гор\.|город[ае]?|с\.|село|сел[ае]|пос\.|поселк[ае]|поселок|п\.|пгт|дер\.|деревн[яеи]|д\.|ст\.|станиц[ае]|хутор[е]?|аул[е]?|рп|г\b)\s*";
const WORD: &str = r"^[а-я][а-я\-]*";
const STOP_WORDS: &[&str] = &[
    "в",
    "во",
    "на",
    "и",
    "а",
    "но",
    "году",
    "года",
    "год",
    "семье",
    "семьи",
    "роддоме",
    "больнице",
    "период",
    "время",
];

fn skip_separators(lower: &str, mut pos: usize) -> usize {
    // The cue may match only the stem of a word (`гражданств` in `гражданство`).
    while let Some(c) = lower[pos..].chars().next() {
        if c.is_alphabetic() {
            pos += c.len_utf8();
        } else {
            break;
        }
    }
    while let Some(c) = lower[pos..].chars().next() {
        if c == ' ' || c == ':' || c == '-' || c == '\t' {
            pos += c.len_utf8();
        } else {
            break;
        }
    }
    pos
}

fn hit_starting_near(places: &[PlaceHit], pos: usize, slack: usize) -> Option<&PlaceHit> {
    places
        .iter()
        .find(|h| h.start >= pos && h.start <= pos + slack)
}

/// `место рождения: г. Москва`, `родился в Казани`, `уроженец с. Ивановка`.
pub struct BirthPlaceDetector {
    cue: Regex,
    marker: Regex,
    word: Regex,
    date: Regex,
    confidence: f32,
}

impl BirthPlaceDetector {
    pub fn new(cues: &[String], confidence: f32) -> Result<Option<Self>> {
        let Some(cue) = compile_alternation(cues, pii::BIRTH_PLACE)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            cue,
            marker: Regex::new(SETTLEMENT_MARKER).expect("static regex"),
            word: Regex::new(WORD).expect("static regex"),
            date: Regex::new(r"^(?:\d{1,2}[./\-]\d{1,2}[./\-]\d{2,4}|\d{4}[./\-]\d{1,2}[./\-]\d{1,2}|\d{1,2}(?:-?го)?\s+[а-я]+\s+\d{4}|[а-я]+\s+\d{1,2},?\s+\d{4})\s*(?:г\.|года|г\b)?[\s,]*").expect("static regex"),
            confidence,
        }))
    }

    /// Up to three capitalised Cyrillic words starting at `pos`.
    fn capitalised_run(&self, ctx: &DetectCtx<'_>, pos: usize) -> Option<usize> {
        let lower = ctx.norm.lower.as_str();
        let mut p = pos;
        let mut end = None;
        for _ in 0..3 {
            let rest = &lower[p..];
            let Some(m) = self.word.find(rest) else { break };
            let word = m.as_str();
            if STOP_WORDS.contains(&word) {
                break;
            }
            let orig = ctx.norm.to_orig_start(p + m.start());
            let cap = ctx.text[orig..]
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false);
            if !cap {
                break;
            }
            end = Some(p + m.end());
            p += m.end();
            if !lower[p..].starts_with(' ') {
                break;
            }
            p += 1;
        }
        end
    }
}

impl Detector for BirthPlaceDetector {
    fn name(&self) -> &'static str {
        "birth_place"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        for m in self.cue.find_iter(lower) {
            let mut pos = skip_separators(lower, m.end());
            // `родился 12 мая 1987 г. в г. Твери`: the date sits between the cue and the place.
            if let Some(d) = self.date.find(&lower[pos..]) {
                if lower[pos + d.end()..].starts_with("в ")
                    || lower[pos + d.end()..].starts_with("во ")
                {
                    pos += d.end();
                }
            }
            for prep in ["во ", "в "] {
                if lower[pos..].starts_with(prep) {
                    pos += prep.len();
                }
            }
            let mut start = pos;
            let mut evidence = "cue+gazetteer";
            let mut end = None;
            if let Some(mm) = self.marker.find(&lower[pos..]) {
                pos += mm.end();
                evidence = "cue+marker";
            } else {
                start = pos;
            }
            if let Some(hit) = hit_starting_near(ctx.places, pos, 0) {
                let mut e = hit.end;
                // `г. Мытищи, Московская область`
                let rest = &lower[e..];
                let trimmed = rest.trim_start_matches([',', ' ']);
                let next_pos = e + (rest.len() - trimmed.len());
                if let Some(h2) = hit_starting_near(ctx.places, next_pos, 0) {
                    if h2.kind != PlaceKind::City && next_pos - e <= 3 {
                        e = h2.end;
                    }
                }
                end = Some(e);
            } else if evidence == "cue+marker" {
                end = self
                    .capitalised_run(ctx, pos)
                    .or_else(|| self.word.find(&lower[pos..]).map(|w| pos + w.end()));
            } else if let Some(e) = self.capitalised_run(ctx, pos) {
                end = Some(e);
                evidence = "cue+capitalised";
            }
            if let Some(end) = end {
                if end > start {
                    let conf = if evidence == "cue+capitalised" {
                        self.confidence - 0.15
                    } else {
                        self.confidence
                    };
                    out.push(
                        Candidate::new(
                            ctx.norm.to_orig_span(start, end),
                            pii::BIRTH_PLACE,
                            conf,
                            "birth_place",
                        )
                        .with_evidence(evidence),
                    );
                }
            }
        }
    }
}

/// `гражданство: Российская Федерация`, `гражданин России`, `российское гражданство`.
pub struct CitizenshipDetector {
    cue: Regex,
    confidence: f32,
}

impl CitizenshipDetector {
    pub fn new(cues: &[String], confidence: f32) -> Result<Option<Self>> {
        Ok(compile_alternation(cues, pii::CITIZENSHIP)?.map(|cue| Self { cue, confidence }))
    }
}

impl Detector for CitizenshipDetector {
    fn name(&self) -> &'static str {
        "citizenship"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        for m in self.cue.find_iter(lower) {
            let pos = skip_separators(lower, m.end());
            let after = ctx
                .places
                .iter()
                .find(|h| h.kind == PlaceKind::Country && h.start >= pos && h.start <= pos + 1);
            let before = ctx.places.iter().find(|h| {
                h.kind == PlaceKind::Country && h.end <= m.start() && m.start() - h.end <= 1
            });
            if let Some(hit) = after.or(before) {
                out.push(
                    Candidate::new(
                        ctx.norm.to_orig_span(hit.start, hit.end),
                        pii::CITIZENSHIP,
                        self.confidence,
                        "citizenship",
                    )
                    .with_evidence("cue+country"),
                );
            }
        }
    }
}
