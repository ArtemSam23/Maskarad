//! The authority that issued a passport: `выдан ОВД района Хамовники г. Москвы`.

use super::context::compile_alternation;
use super::{DetectCtx, Detector};
use crate::error::Result;
use crate::types::{pii, Candidate};
use regex::Regex;

const ANCHOR: &str = r"\b(?:овд|оуфмс|уфмс|офмс|гу\s+мвд|мвд|омвд|умвд|овм|тп\s*№?\s*\d+|гувд|увд|фмс|мфц|паспортн\w+\s+стол\w*|отдел\w*\s+(?:внутренних|полиции|милиции|уфмс|офмс|мвд|по\s+вопросам\s+миграции)|отделени\w+\s+(?:уфмс|офмс|мвд|полиции|милиции|по\s+вопросам\s+миграции))";
const STOP: &str = r"\d{2}[./\-]\d{2}[./\-]\d{2,4}|\b\d{1,2}\s+(?:января|февраля|марта|апреля|мая|июня|июля|августа|сентября|октября|ноября|декабря)\b|\b(?:января|февраля|марта|апреля|мая|июня|июля|августа|сентября|октября|ноября|декабря)\s+\d{1,2},?\s+\d{4}|код\s+подр|к/п|\bкп\s*[:\-]?\s*\d|;|\n|\bпаспорт|\bсерия|\bномер\s+паспорта|\bпрожива|\bзарегистрирован|\bпрописан|\bадрес|\bтел\b|\bтелефон|\bинн\b|\bснилс|\bдата\b|\bгражданств|\bместо\s+рождения|\bд\.р\.|\bродил|\bemail|\be-mail|\bпочта|\bкарт[аы]\b";
const ABBREVIATIONS: &[&str] = &[
    "г", "гор", "р-н", "ул", "п", "пос", "обл", "респ", "с", "д", "им", "св", "ст", "т", "тп",
    "отд", "п-ов", "м", "мкр",
];

pub struct IssuerDetector {
    cue: Option<Regex>,
    anchor: Regex,
    stop: Regex,
    date: Regex,
    confidence: f32,
}

impl IssuerDetector {
    pub fn new(cues: &[String], confidence: f32) -> Result<Self> {
        Ok(Self {
            cue: compile_alternation(cues, pii::PASSPORT_ISSUER)?,
            anchor: Regex::new(ANCHOR).expect("static regex"),
            stop: Regex::new(STOP).expect("static regex"),
            date: Regex::new(r"^\s*(?:\d{2}[./\-]\d{2}[./\-]\d{2,4}|\d{4}[./\-]\d{2}[./\-]\d{2}|\d{1,2}\s+(?:января|февраля|марта|апреля|мая|июня|июля|августа|сентября|октября|ноября|декабря)\s+\d{4}|(?:января|февраля|марта|апреля|мая|июня|июля|августа|сентября|октября|ноября|декабря)\s+\d{1,2},?\s+\d{4})\s*(?:г\.|года|г\b)?[\s,]*")
                .expect("static regex"),
            confidence,
        })
    }

    /// End of the authority phrase starting at `start`.
    fn phrase_end(&self, ctx: &DetectCtx<'_>, start: usize) -> usize {
        let lower = ctx.norm.lower.as_str();
        let limit = {
            let mut e = (start + 160).min(lower.len());
            while !lower.is_char_boundary(e) {
                e += 1;
            }
            e
        };
        let segment = &lower[start..limit];
        let mut end = self
            .stop
            .find(segment)
            .map(|m| start + m.start())
            .unwrap_or(limit);
        // A sentence-ending period: `. ` followed by an upper-case letter in the
        // original, unless the word before the period is an abbreviation.
        let mut search = start;
        while let Some(rel) = lower[search..end].find(". ") {
            let dot = search + rel;
            let word_before = lower[start..dot]
                .rsplit(|c: char| !c.is_alphanumeric() && c != '-')
                .next()
                .unwrap_or("");
            let after_ws =
                lower[dot + 1..].trim_start().as_ptr() as usize - lower.as_ptr() as usize;
            let next_upper = ctx.text[ctx.norm.to_orig_start(after_ws)..]
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false);
            if !ABBREVIATIONS.contains(&word_before) && next_upper {
                end = dot;
                break;
            }
            search = dot + 2;
        }
        // Trim trailing separators.
        while end > start {
            let c = lower[..end].chars().next_back().unwrap();
            if c == ' ' || c == ',' || c == '.' || c == ':' || c == ';' || c == '-' {
                end -= c.len_utf8();
            } else {
                break;
            }
        }
        end
    }

    fn push(
        &self,
        ctx: &DetectCtx<'_>,
        start: usize,
        end: usize,
        conf: f32,
        evidence: &'static str,
        out: &mut Vec<Candidate>,
    ) {
        let lower = ctx.norm.lower.as_str();
        let phrase = &lower[start..end];
        if phrase.chars().count() < 3 || !phrase.chars().any(|c| c.is_alphabetic()) {
            return;
        }
        out.push(
            Candidate::new(
                ctx.norm.to_orig_span(start, end),
                pii::PASSPORT_ISSUER,
                conf,
                "issuer",
            )
            .with_evidence(evidence),
        );
    }
}

impl Detector for IssuerDetector {
    fn name(&self) -> &'static str {
        "issuer"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        if let Some(cue) = &self.cue {
            for m in cue.find_iter(lower) {
                let mut pos = m.end();
                let rest = &lower[pos..];
                pos += rest.len() - rest.trim_start_matches([' ', ':', '-']).len();
                if let Some(d) = self.date.find(&lower[pos..]) {
                    pos += d.end();
                }
                if !lower[pos..]
                    .chars()
                    .next()
                    .map(|c| c.is_alphabetic() || c == '«' || c == '"')
                    .unwrap_or(false)
                {
                    continue;
                }
                let end = self.phrase_end(ctx, pos);
                self.push(ctx, pos, end, self.confidence, "cue+phrase", out);
            }
        }
        for m in self.anchor.find_iter(lower) {
            let end = self.phrase_end(ctx, m.start());
            self.push(
                ctx,
                m.start(),
                end,
                self.confidence - 0.1,
                "anchor+phrase",
                out,
            );
        }
    }
}
