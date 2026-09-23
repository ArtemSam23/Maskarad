//! Postal addresses assembled from components: country, postal index, region,
//! settlement, street, house, apartment. Components are found by markers
//! (`ул.`, `д.`, `кв.`, …) and by the place gazetteer, then chained by
//! proximity. The whole chain becomes an `ADDRESS` candidate and each
//! component a child candidate.

use super::context::{window_before, ContextMatcher};
use super::{digit_boundary, DetectCtx, Detector};
use crate::config::ContextSpec;
use crate::dict::PlaceKind;
use crate::error::Result;
use crate::types::{pii, Candidate};
use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Country,
    Index,
    Region,
    City,
    Street,
    House,
    Apartment,
}

impl Kind {
    fn pii(self) -> &'static str {
        match self {
            Kind::Country => pii::ADDRESS_COUNTRY,
            Kind::Index => pii::ADDRESS_INDEX,
            Kind::Region => pii::ADDRESS_REGION,
            Kind::City => pii::ADDRESS_CITY,
            Kind::Street => pii::ADDRESS_STREET,
            Kind::House => pii::ADDRESS_HOUSE,
            Kind::Apartment => pii::ADDRESS_APARTMENT,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Comp {
    start: usize,
    end: usize,
    kind: Kind,
    /// Found with an explicit marker (`г.`, `ул.`) rather than by name only.
    strong: bool,
}

/// Street-type words that are also common street names (`ул. Набережная`).
const STREET_TYPE_NAMES: &[&str] = &[
    "набережная",
    "проспект",
    "площадь",
    "бульвар",
    "шоссе",
    "аллея",
    "линия",
    "тракт",
    "дорога",
    "квартал",
    "микрорайон",
    "проезд",
    "переулок",
];

const MARKER_WORDS: &[&str] = &[
    "г",
    "гор",
    "город",
    "с",
    "село",
    "пос",
    "поселок",
    "п",
    "пгт",
    "дер",
    "деревня",
    "д",
    "дом",
    "ст",
    "станица",
    "рп",
    "ул",
    "улица",
    "пр-т",
    "пр",
    "просп",
    "проспект",
    "пер",
    "переулок",
    "наб",
    "набережная",
    "б-р",
    "бул",
    "бульвар",
    "ш",
    "шоссе",
    "пл",
    "площадь",
    "пр-д",
    "проезд",
    "аллея",
    "туп",
    "тупик",
    "линия",
    "мкр",
    "микрорайон",
    "кв-л",
    "квартал",
    "тракт",
    "дорога",
    "кв",
    "квартира",
    "оф",
    "офис",
    "пом",
    "помещение",
    "комн",
    "каб",
    "корп",
    "к",
    "корпус",
    "стр",
    "строение",
    "лит",
    "литера",
    "обл",
    "область",
    "край",
    "респ",
    "республика",
    "р-н",
    "район",
    "индекс",
    "адрес",
    "россия",
    "рф",
];

pub struct AddressDetector {
    ctx: ContextMatcher,
    index: Regex,
    index_marked: Regex,
    region_marker: Regex,
    settlement: Regex,
    street: Regex,
    street_before: Regex,
    numbered_line: Regex,
    house: Regex,
    house_bare: Regex,
    apartment: Regex,
    word: Regex,
    confidence: f32,
}

impl AddressDetector {
    pub fn new(spec: &ContextSpec, default_window: usize, confidence: f32) -> Result<Self> {
        let re = |s: &str| Regex::new(s).expect("static regex");
        Ok(Self {
            ctx: ContextMatcher::compile(spec, default_window, pii::ADDRESS)?,
            index: re(r"\b[1-6]\d{5}\b"),
            index_marked: re(r"индекс\s*[:\-]?\s*(?P<v>\d{6})\b"),
            region_marker: re(
                r"\b(?:обл\.|область|обл\b|края|край|респ\.|республик[аи]|ао\b|автономн\w+\s+округ\w*|р-н|район[ае]?)",
            ),
            settlement: re(
                r"\b(?:г\.|гор\.|город[ае]?|с\.|село|сел[ае]|пос\.|поселк[ае]|поселок|п\.|пгт|дер\.|деревн[яеи]|д\.|ст\.|станиц[ае]|рп|хутор[е]?|аул[е]?)\s*",
            ),
            street: re(
                r"\b(?:ул\.|улиц[аеы]|ул\b|пр-т|пр\.|просп\.|проспект[ае]?|пер\.|переул\w+|наб\.|набережн\w+|б-р|бул\.|бульвар[ае]?|ш\.|шоссе|пл\.|площад[ьи]|пр-д|проезд[ае]?|алле[яе]|туп\.|тупик[ае]?|мкр\.|мкр\b|микрорайон[ае]?|кв-л|квартал[ае]?|тракт[ае]?|дорог[аеи]|въезд[ае]?|спуск[ае]?)\s*",
            ),
            street_before: re(r"([а-я][а-я\-]+(?:\s+[а-я][а-я\-]+)?)\s*$"),
            numbered_line: re(r"\b\d{1,2}-?[яй]\s+лини[яи](?:\s+в\.?\s?о\.?)?"),
            house: re(
                r"\b(?:д\.|дом[ае]?|д\b|№|n)\s*(?P<v>\d+\s?[а-я]?(?:/\d+)?)\b(?:[\s,]*(?:к\.|корп\.|корпус[ае]?|к\b)\s*\d+[а-я]?)?(?:[\s,]*(?:стр\.|строени[ея]|с\b|ст\.)\s*\d+)?(?:[\s,]*(?:лит\.|литер[аы]|лит\b)\s*[а-я]\b)?",
            ),
            house_bare: re(
                r"^[\s,]*(?P<v>\d{1,4}\s?[а-я]?(?:/\d+)?)\b(?:[\s,]*(?:к\.|корп\.|корпус[ае]?|к\b)\s*\d+[а-я]?)?(?:[\s,]*(?:стр\.|строени[ея]|с\b|ст\.)\s*\d+)?",
            ),
            apartment: re(
                r"\b(?:кв\.|квартир[аеы]|кв\b|оф\.|офис[ае]?|пом\.|помещени[ея]|комн\.|каб\.|ком\.)\s*(?P<v>\d+[а-я]?)\b",
            ),
            word: re(r"^[а-я0-9][а-я0-9\-]*"),
            confidence,
        })
    }

    /// Name words after a marker: up to three words, stopping at punctuation,
    /// another marker or a house number.
    fn name_after(&self, lower: &str, pos: usize, allow_leading_number: bool) -> Option<usize> {
        let mut p = pos;
        let mut end = None;
        let mut count = 0;
        while count < 3 {
            let rest = &lower[p..];
            let Some(m) = self.word.find(rest) else { break };
            let w = m.as_str();
            let digits = w.chars().all(|c| c.is_ascii_digit());
            if digits {
                if count == 0 && allow_leading_number {
                    let after = lower[p + m.end()..].trim_start();
                    let month_or_year = [
                        "января",
                        "февраля",
                        "марта",
                        "апреля",
                        "мая",
                        "июня",
                        "июля",
                        "августа",
                        "сентября",
                        "октября",
                        "ноября",
                        "декабря",
                        "года",
                        "лет",
                        "-й",
                        "-я",
                        "-го",
                    ]
                    .iter()
                    .any(|s| after.starts_with(s));
                    if !month_or_year {
                        break;
                    }
                } else {
                    break;
                }
            } else if MARKER_WORDS.contains(&w) && !(count == 0 && STREET_TYPE_NAMES.contains(&w)) {
                break;
            }
            count += 1;
            end = Some(p + m.end());
            p += m.end();
            match lower[p..].chars().next() {
                Some(' ') => p += 1,
                Some('.') if lower[p + 1..].starts_with(' ') => {
                    // `ул. Пушкина. Дом` — a sentence break
                    break;
                }
                _ => break,
            }
        }
        end
    }

    fn components(&self, ctx: &DetectCtx<'_>) -> Vec<Comp> {
        let lower = ctx.norm.lower.as_str();
        let mut comps: Vec<Comp> = Vec::new();
        for m in self.index.find_iter(lower) {
            if digit_boundary(lower, m.start(), m.end()) {
                comps.push(Comp {
                    start: m.start(),
                    end: m.end(),
                    kind: Kind::Index,
                    strong: false,
                });
            }
        }
        for c in self.index_marked.captures_iter(lower) {
            let v = c.name("v").unwrap();
            comps.push(Comp {
                start: v.start(),
                end: v.end(),
                kind: Kind::Index,
                strong: true,
            });
        }
        for hit in ctx.places {
            match hit.kind {
                PlaceKind::Country => {
                    let t = &lower[hit.start..hit.end];
                    let adjective = ["ое", "ая", "ий", "ой", "ые", "ого", "ой"]
                        .iter()
                        .any(|s| t.ends_with(s));
                    if !adjective {
                        comps.push(Comp {
                            start: hit.start,
                            end: hit.end,
                            kind: Kind::Country,
                            strong: false,
                        });
                    }
                }
                PlaceKind::City => comps.push(Comp {
                    start: hit.start,
                    end: hit.end,
                    kind: Kind::City,
                    strong: false,
                }),
                PlaceKind::Region => {
                    let after = &lower[hit.end..];
                    let after_trim = after.trim_start();
                    let before = window_before(lower, hit.start, 24);
                    let before_trim = before.trim_end();
                    let mut start = hit.start;
                    let mut end = hit.end;
                    let mut marked = false;
                    if let Some(m) = self.region_marker.find(after_trim) {
                        if m.start() == 0 {
                            end = hit.end + (after.len() - after_trim.len()) + m.end();
                            marked = true;
                        }
                    }
                    if !marked {
                        if let Some(m) = self.region_marker.find_iter(before_trim).last() {
                            if m.end() == before_trim.len() {
                                start = hit.start
                                    - (before.len() - before_trim.len())
                                    - (before_trim.len() - m.start());
                                marked = true;
                            }
                        }
                    }
                    if marked {
                        comps.push(Comp {
                            start,
                            end,
                            kind: Kind::Region,
                            strong: true,
                        });
                    }
                }
            }
        }
        for m in self.settlement.find_iter(lower) {
            let marker = m.as_str().trim_end();
            let after = &lower[m.end()..];
            let next_is_letter = after
                .chars()
                .next()
                .map(|c| c.is_alphabetic())
                .unwrap_or(false);
            if (marker == "д." || marker == "д") && !next_is_letter {
                continue;
            }
            if let Some(end) = self.name_after(lower, m.end(), false) {
                comps.push(Comp {
                    start: m.start(),
                    end,
                    kind: Kind::City,
                    strong: true,
                });
            }
        }
        for m in self.street.find_iter(lower) {
            if let Some(end) = self.name_after(lower, m.end(), true) {
                comps.push(Comp {
                    start: m.start(),
                    end,
                    kind: Kind::Street,
                    strong: true,
                });
            } else if let Some(c) = self
                .street_before
                .captures(window_before(lower, m.start(), 64))
            {
                let g = c.get(1).unwrap();
                let base = m.start() - window_before(lower, m.start(), 64).len();
                let name = g.as_str();
                let first_word = name.split(' ').next().unwrap_or("");
                let allowed =
                    |w: &str| !MARKER_WORDS.contains(&w) || STREET_TYPE_NAMES.contains(&w);
                if allowed(first_word) && allowed(name) {
                    let end = m.start() + m.as_str().trim_end().len();
                    comps.push(Comp {
                        start: base + g.start(),
                        end,
                        kind: Kind::Street,
                        strong: true,
                    });
                }
            }
        }
        for m in self.numbered_line.find_iter(lower) {
            comps.push(Comp {
                start: m.start(),
                end: m.end(),
                kind: Kind::Street,
                strong: true,
            });
        }
        for m in self.house.find_iter(lower) {
            comps.push(Comp {
                start: m.start(),
                end: m.end(),
                kind: Kind::House,
                strong: true,
            });
        }
        for m in self.apartment.find_iter(lower) {
            comps.push(Comp {
                start: m.start(),
                end: m.end(),
                kind: Kind::Apartment,
                strong: true,
            });
        }
        // A bare number right after a street name is the house.
        let streets: Vec<Comp> = comps
            .iter()
            .filter(|c| c.kind == Kind::Street)
            .copied()
            .collect();
        for s in streets {
            if let Some(c) = self.house_bare.captures(&lower[s.end..]) {
                let whole = c.get(0).unwrap();
                let v = c.name("v").unwrap();
                let start = s.end + v.start();
                let end = s.end + whole.end();
                if !comps
                    .iter()
                    .any(|c| c.kind == Kind::House && c.start < end && start < c.end)
                {
                    comps.push(Comp {
                        start,
                        end,
                        kind: Kind::House,
                        strong: true,
                    });
                }
            }
        }
        comps.sort_by_key(|c| (c.start, std::cmp::Reverse(c.end)));
        // Drop components nested in or overlapping a marker-based one.
        let mut kept: Vec<Comp> = Vec::new();
        for c in comps {
            if let Some(last) = kept.last() {
                if c.start < last.end {
                    if c.strong && !last.strong && c.end >= last.end {
                        kept.pop();
                    } else {
                        continue;
                    }
                }
            }
            kept.push(c);
        }
        kept
    }
}

fn gap_ok(lower: &str, a: &Comp, b: &Comp) -> bool {
    let gap = &lower[a.end..b.start];
    gap.chars().count() <= 4
        && gap
            .chars()
            .all(|c| c == ' ' || c == ',' || c == ';' || c == '.')
}

impl Detector for AddressDetector {
    fn name(&self) -> &'static str {
        "address"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        let comps = self.components(ctx);
        let mut i = 0;
        while i < comps.len() {
            let mut j = i + 1;
            while j < comps.len() && gap_ok(lower, &comps[j - 1], &comps[j]) {
                j += 1;
            }
            let chain = &comps[i..j];
            let has = |k: Kind, strong_only: bool| {
                chain
                    .iter()
                    .any(|c| c.kind == k && (!strong_only || c.strong))
            };
            let strong = (has(Kind::Street, false)
                && (has(Kind::House, false) || has(Kind::City, false) || has(Kind::Index, false)))
                || (has(Kind::City, true)
                    && (has(Kind::House, false)
                        || has(Kind::Index, false)
                        || has(Kind::Region, false)))
                || (has(Kind::Index, false)
                    && (has(Kind::City, false) || has(Kind::Region, false)))
                || (has(Kind::Region, false) && has(Kind::City, false) && has(Kind::House, false));
            let (start, end) = (chain[0].start, chain[chain.len() - 1].end);
            let cr = self.ctx.check(lower, start, end);
            if cr.suppressed {
                i = j;
                continue;
            }
            if strong || (cr.matched && chain.len() >= 2) || (cr.matched && has(Kind::Street, true))
            {
                let conf = if cr.matched {
                    (self.confidence + 0.05).min(1.0)
                } else {
                    self.confidence
                };
                let evidence = if cr.matched {
                    "components+context"
                } else {
                    "components"
                };
                out.push(
                    Candidate::new(
                        ctx.norm.to_orig_span(start, end),
                        pii::ADDRESS,
                        conf,
                        "address",
                    )
                    .with_evidence(evidence),
                );
                for c in chain {
                    out.push(
                        Candidate::new(
                            ctx.norm.to_orig_span(c.start, c.end),
                            c.kind.pii(),
                            conf,
                            "address",
                        )
                        .with_evidence("component"),
                    );
                }
            } else if chain.len() == 1 {
                let c = chain[0];
                let weak_kind = match c.kind {
                    Kind::City if c.strong => Some(pii::ADDRESS_CITY),
                    Kind::Index if c.strong => Some(pii::ADDRESS_INDEX),
                    Kind::Street if c.strong => Some(pii::ADDRESS_STREET),
                    _ => None,
                };
                if let Some(kind) = weak_kind {
                    let conf = if cr.matched { 0.8 } else { 0.55 };
                    let mut cand = Candidate::new(
                        ctx.norm.to_orig_span(c.start, c.end),
                        kind,
                        conf,
                        "address",
                    )
                    .with_evidence("lone_component");
                    if !cr.matched {
                        cand = cand.weak();
                    }
                    out.push(cand);
                }
            }
            i = j;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::gazetteer::Gazetteer;
    use crate::detect::tokens::tokenize;
    use crate::dict::Dictionaries;
    use crate::normalize::Normalized;

    fn run(text: &str) -> Vec<(String, String)> {
        let dict = Dictionaries::builtin();
        let gaz = Gazetteer::build(&dict).unwrap();
        let norm = Normalized::new(text);
        let words = tokenize(text, &norm);
        let places = gaz.find(&norm.lower);
        let ctx = DetectCtx {
            text,
            norm: &norm,
            words: &words,
            dict: &dict,
            places: &places,
            window: 48,
        };
        let det = AddressDetector::new(&ContextSpec::default(), 48, 0.9).unwrap();
        let mut out = Vec::new();
        det.detect(&ctx, &mut out);
        out.into_iter()
            .map(|c| (c.ty.to_string(), text[c.span.start..c.span.end].to_string()))
            .collect()
    }

    #[test]
    fn full_address_with_components() {
        let r = run("Адрес: 101000, Россия, г. Москва, ул. Тверская, д. 7, корп. 2, кв. 12.");
        assert!(
            r.contains(&(
                "ADDRESS".into(),
                "101000, Россия, г. Москва, ул. Тверская, д. 7, корп. 2, кв. 12".into()
            )),
            "{r:?}"
        );
        assert!(r.contains(&("ADDRESS_STREET".into(), "ул. Тверская".into())));
        assert!(r.contains(&("ADDRESS_HOUSE".into(), "д. 7, корп. 2".into())));
        assert!(r.contains(&("ADDRESS_APARTMENT".into(), "кв. 12".into())));
        assert!(r.contains(&("ADDRESS_CITY".into(), "г. Москва".into())));
    }

    #[test]
    fn street_name_before_marker_and_bare_house() {
        let r = run("проживает: Тверская ул. 7, Москва");
        assert!(
            r.iter()
                .any(|(t, s)| t == "ADDRESS" && s.starts_with("Тверская ул. 7")),
            "{r:?}"
        );
    }

    #[test]
    fn lone_city_is_weak() {
        let r = run("Офис открылся в г. Москва");
        assert_eq!(
            r,
            vec![("ADDRESS_CITY".to_string(), "г. Москва".to_string())]
        );
    }
}
