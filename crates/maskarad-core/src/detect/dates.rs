//! Dates in every spelling the specification mentions: numeric with any
//! order of day/month/year, month written as a word, day and year written as
//! words. The context decides whether a date is a birth date, a passport
//! issue date or a plain date (weak).

use super::context::{compile_alternation, window_after, window_before};
use super::{digit_boundary, DetectCtx, Detector};
use crate::error::Result;
use crate::types::{pii, Candidate};
use regex::Regex;

const MONTHS_GEN: &str =
    "января|февраля|марта|апреля|мая|июня|июля|августа|сентября|октября|ноября|декабря";
const MONTHS_NOM: &str =
    "январь|февраль|март|апрель|май|июнь|июль|август|сентябрь|октябрь|ноябрь|декабрь";
const MONTHS_ABBR: &str = "янв|фев|февр|мар|апр|июн|июл|авг|сен|сент|окт|ноя|нояб|дек";

const DAY_WORDS: &str = "(?:первого|второго|третьего|четвертого|пятого|шестого|седьмого|восьмого|девятого|десятого|одиннадцатого|двенадцатого|тринадцатого|четырнадцатого|пятнадцатого|шестнадцатого|семнадцатого|восемнадцатого|девятнадцатого|двадцатого|тридцатого|(?:двадцать|тридцать)\\s+(?:первого|второго|третьего|четвертого|пятого|шестого|седьмого|восьмого|девятого))";
const YEAR_WORDS: &str = "(?:(?:одна\\s+)?тысяча|две\\s+тысячи|двухтысячного)(?:\\s+(?:сто|двести|триста|четыреста|пятьсот|шестьсот|семьсот|восемьсот|девятьсот))?(?:\\s+(?:десять|одиннадцать|двенадцать|тринадцать|четырнадцать|пятнадцать|шестнадцать|семнадцать|восемнадцать|девятнадцать|двадцать|тридцать|сорок|пятьдесят|шестьдесят|семьдесят|восемьдесят|девяносто))?(?:\\s+(?:первого|второго|третьего|четвертого|пятого|шестого|седьмого|восьмого|девятого|десятого|одиннадцатого|двенадцатого|тринадцатого|четырнадцатого|пятнадцатого|шестнадцатого|семнадцатого|восемнадцатого|девятнадцатого|двадцатого|тридцатого|сорокового|пятидесятого|шестидесятого|семидесятого|восьмидесятого|девяностого|сотого|тысячного|двухтысячного))?";

#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `12.05.1987` or `05.12.1987`
    NumericDmy,
    /// `1987-05-12` or `1987.12.05`
    NumericYmd,
    /// `12.05.87` — only accepted with a birth/issue cue
    NumericShort,
    /// `12 мая 1987`
    TextDmy,
    /// `мая 12, 1987`
    TextMdy,
    /// `1987 год, 12 мая`
    TextYdm,
    /// `двенадцатого мая тысяча девятьсот восемьдесят седьмого`
    Words,
}

pub struct DateConfig {
    pub birth_before: Vec<String>,
    pub birth_after: Vec<String>,
    pub issue_before: Vec<String>,
    pub window: usize,
    pub birth_confidence: f32,
    pub issue_confidence: f32,
    pub plain_confidence: f32,
    pub birth_enabled: bool,
    pub issue_enabled: bool,
    pub plain_enabled: bool,
}

impl Default for DateConfig {
    fn default() -> Self {
        Self {
            birth_before: vec![
                "дата\\s+рождения".into(),
                "д\\.\\s?р\\.".into(),
                "д/р".into(),
                "родил(?:ся|ась|ись)".into(),
                "рожден".into(),
                "день\\s+рождения".into(),
                "birth".into(),
                "born".into(),
            ],
            birth_after: vec!["^\\s*(?:г\\.?\\s?р\\.?|года\\s+рождения|г\\.\\s*рождения)".into()],
            issue_before: vec![
                "выдан".into(),
                "дата\\s+выдачи".into(),
                "выдач".into(),
                "issued".into(),
            ],
            window: 40,
            birth_confidence: 0.95,
            issue_confidence: 0.95,
            plain_confidence: 0.6,
            birth_enabled: true,
            issue_enabled: true,
            plain_enabled: true,
        }
    }
}

pub struct DateDetector {
    shapes: Vec<(Regex, Shape)>,
    birth_before: Option<Regex>,
    birth_after: Option<Regex>,
    issue_before: Option<Regex>,
    cfg: DateConfig,
}

fn shape_regexes() -> Vec<(String, Shape)> {
    // ASCII word boundaries keep the regex engine on its DFA for Cyrillic
    // haystacks; every boundary here is next to a digit, where ASCII and
    // Unicode boundaries agree. Shapes that start with a word consume one
    // separator character before the `v` group instead.
    let m = format!("(?P<m>(?:{MONTHS_GEN}|{MONTHS_NOM}|(?:{MONTHS_ABBR})\\.?))");
    vec![
        (
            r"(?-u:\b)(?P<a>\d{1,2})[./\-](?P<b>\d{1,2})[./\-](?P<y>\d{4})(?-u:\b)".to_string(),
            Shape::NumericDmy,
        ),
        (
            r"(?-u:\b)(?P<y>\d{4})[./\-](?P<a>\d{1,2})[./\-](?P<b>\d{1,2})(?-u:\b)".to_string(),
            Shape::NumericYmd,
        ),
        (
            r"(?-u:\b)(?P<a>\d{2})[./](?P<b>\d{2})[./](?P<y2>\d{2})(?-u:\b)".to_string(),
            Shape::NumericShort,
        ),
        (
            format!(r"(?-u:\b)(?P<d>\d{{1,2}})(?:-?го)?\s+{m}\s+(?P<y>\d{{4}})(?-u:\b)"),
            Shape::TextDmy,
        ),
        (
            format!(r"(?:^|[^а-яa-z0-9])(?P<v>{m}\s+(?P<d>\d{{1,2}}),?\s+(?P<y>\d{{4}}))(?-u:\b)"),
            Shape::TextMdy,
        ),
        (
            format!(
                r"(?-u:\b)(?P<y>\d{{4}})\s*(?:г\.|года|год)?,?\s*(?P<d>\d{{1,2}})\s+{m}(?:[^а-я]|$)"
            ),
            Shape::TextYdm,
        ),
        (
            format!(
                r"(?:^|[^а-яa-z0-9])(?P<v>(?P<dw>{DAY_WORDS})\s+(?P<mw>{MONTHS_GEN})\s+(?P<yw>{YEAR_WORDS}))(?:[^а-я]|$)"
            ),
            Shape::Words,
        ),
    ]
}

fn month_number(s: &str) -> Option<u32> {
    let s = s.trim_end_matches('.');
    let idx = |list: &str| list.split('|').position(|m| m == s).map(|i| i as u32 + 1);
    idx(MONTHS_GEN).or_else(|| idx(MONTHS_NOM)).or(match s {
        "янв" => Some(1),
        "фев" | "февр" => Some(2),
        "мар" => Some(3),
        "апр" => Some(4),
        "июн" => Some(6),
        "июл" => Some(7),
        "авг" => Some(8),
        "сен" | "сент" => Some(9),
        "окт" => Some(10),
        "ноя" | "нояб" => Some(11),
        "дек" => Some(12),
        _ => None,
    })
}

pub fn valid_ymd(y: u32, m: u32, d: u32) -> bool {
    if !(1800..=2099).contains(&y) || !(1..=12).contains(&m) || d == 0 {
        return false;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if leap {
                29
            } else {
                28
            }
        }
    };
    d <= days
}

fn num(caps: &regex::Captures<'_>, name: &str) -> Option<u32> {
    caps.name(name).and_then(|m| m.as_str().parse().ok())
}

fn valid_capture(caps: &regex::Captures<'_>, shape: Shape) -> bool {
    match shape {
        Shape::NumericDmy | Shape::NumericYmd => {
            let (Some(a), Some(b), Some(y)) = (num(caps, "a"), num(caps, "b"), num(caps, "y"))
            else {
                return false;
            };
            valid_ymd(y, b, a) || valid_ymd(y, a, b)
        }
        Shape::NumericShort => {
            let (Some(a), Some(b), Some(y2)) = (num(caps, "a"), num(caps, "b"), num(caps, "y2"))
            else {
                return false;
            };
            let y = if y2 <= 30 { 2000 + y2 } else { 1900 + y2 };
            valid_ymd(y, b, a) || valid_ymd(y, a, b)
        }
        Shape::TextDmy | Shape::TextMdy | Shape::TextYdm => {
            let (Some(d), Some(y)) = (num(caps, "d"), num(caps, "y")) else {
                return false;
            };
            let Some(m) = caps.name("m").and_then(|m| month_number(m.as_str())) else {
                return false;
            };
            valid_ymd(y, m, d)
        }
        Shape::Words => true,
    }
}

/// Whether the whole string is a date in one of the supported spellings.
pub fn is_valid_date_text(s: &str) -> bool {
    let s = s.trim();
    shape_regexes().into_iter().any(|(src, shape)| {
        let re = Regex::new(&format!("^(?:{src})$")).expect("static date regex");
        re.captures(s)
            .map(|c| valid_capture(&c, shape))
            .unwrap_or(false)
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Birth,
    Issue,
    Plain,
}

impl DateDetector {
    pub fn new(cfg: DateConfig) -> Result<Self> {
        let shapes = shape_regexes()
            .into_iter()
            .map(|(src, shape)| Regex::new(&src).map(|re| (re, shape)))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|source| crate::error::CoreError::Regex {
                ty: "DATE".into(),
                source,
            })?;
        Ok(Self {
            shapes,
            birth_before: compile_alternation(&cfg.birth_before, pii::BIRTH_DATE)?,
            birth_after: compile_alternation(&cfg.birth_after, pii::BIRTH_DATE)?,
            issue_before: compile_alternation(&cfg.issue_before, pii::PASSPORT_ISSUE_DATE)?,
            cfg,
        })
    }

    fn classify(&self, lower: &str, start: usize, end: usize) -> Class {
        let after = window_after(lower, end, 16);
        if self
            .birth_after
            .as_ref()
            .map(|r| r.is_match(after))
            .unwrap_or(false)
        {
            return Class::Birth;
        }
        let before = window_before(lower, start, self.cfg.window);
        let last_end = |re: &Option<Regex>| {
            re.as_ref()
                .and_then(|r| r.find_iter(before).last().map(|m| m.end()))
        };
        match (last_end(&self.birth_before), last_end(&self.issue_before)) {
            (Some(b), Some(i)) if b >= i => Class::Birth,
            (Some(_), Some(_)) => Class::Issue,
            (Some(_), None) => Class::Birth,
            (None, Some(_)) => Class::Issue,
            (None, None) => Class::Plain,
        }
    }
}

impl Detector for DateDetector {
    fn name(&self) -> &'static str {
        "dates"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        for (re, shape) in &self.shapes {
            for m in re.find_iter(lower) {
                let Some(caps) = re.captures(&lower[m.start()..m.end()]) else {
                    continue;
                };
                if !valid_capture(&caps, *shape) {
                    continue;
                }
                // The value is the `v` group when the shape consumes a separator,
                // otherwise the match up to the end of the last date component.
                let (start, end) = match caps.name("v") {
                    Some(v) => (m.start() + v.start(), m.start() + v.end()),
                    None => {
                        let last = ["y", "m", "b"]
                            .iter()
                            .filter_map(|g| caps.name(g))
                            .map(|g| g.end())
                            .max()
                            .unwrap_or(m.end() - m.start());
                        (m.start(), m.start() + last)
                    }
                };
                if !digit_boundary(lower, start, end) {
                    continue;
                }
                let class = self.classify(lower, start, end);
                if matches!(shape, Shape::NumericShort) && class == Class::Plain {
                    continue;
                }
                let span = ctx.norm.to_orig_span(start, end);
                let cand = match class {
                    Class::Birth if self.cfg.birth_enabled => {
                        Candidate::new(span, pii::BIRTH_DATE, self.cfg.birth_confidence, "dates")
                            .with_evidence("date+birth_context")
                    }
                    Class::Issue if self.cfg.issue_enabled => Candidate::new(
                        span,
                        pii::PASSPORT_ISSUE_DATE,
                        self.cfg.issue_confidence,
                        "dates",
                    )
                    .with_evidence("date+issue_context"),
                    _ if self.cfg.plain_enabled => {
                        Candidate::new(span, pii::DATE, self.cfg.plain_confidence, "dates")
                            .with_evidence("date")
                            .weak()
                    }
                    _ => continue,
                };
                out.push(cand);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity_by_shape() {
        assert!(is_valid_date_text("12.05.1987"));
        assert!(is_valid_date_text("05.12.1987"));
        assert!(is_valid_date_text("1987.12.05"));
        assert!(is_valid_date_text("1987-05-12"));
        assert!(is_valid_date_text("12 мая 1987"));
        assert!(is_valid_date_text("12-го мая 1987"));
        assert!(is_valid_date_text("мая 12, 1987"));
        assert!(is_valid_date_text("1987 год, 12 мая"));
        assert!(is_valid_date_text(
            "двенадцатого мая тысяча девятьсот восемьдесят седьмого"
        ));
        assert!(is_valid_date_text(
            "шестого июня тысяча семьсот девяносто девятого"
        ));
        assert!(!is_valid_date_text("31.02.1987"));
        assert!(!is_valid_date_text("13.13.1987"));
        assert!(!is_valid_date_text("12.05.1700"));
    }
}
