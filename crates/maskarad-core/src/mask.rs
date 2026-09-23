//! Masking strategies and identity-consistent replacement.
//!
//! The same value (after normalisation: case, `ё`, spacing, digits only for
//! numbers, lemmas for names) always receives the same placeholder within a
//! text, and — for `token` and `synthetic` — the same replacement across
//! requests of a system sharing a secret.

use crate::config::{MaskChars, Profile, StrategySpec};
use crate::detect::validators;
use crate::dict::{fold, transliterate, Dictionaries};
use crate::inflect;
use crate::sensitive::Sensitive;
use crate::types::{pii, Entity, MaskEntry, MaskResult, PiiType};
use hmac::{Hmac, Mac};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use sha2::Sha256;
use std::collections::HashMap;

type HmacSha256 = Hmac<Sha256>;

const NUMERIC_TYPES: &[&str] = &[
    pii::PHONE,
    pii::CARD_NUMBER,
    pii::INN,
    pii::PASSPORT,
    pii::SNILS,
    pii::SUBDIVISION_CODE,
    pii::DRIVER_LICENSE,
    pii::CVV,
    pii::PIN,
    pii::ADDRESS_INDEX,
    pii::BANK_ACCOUNT,
    pii::OMS_POLICY,
    pii::FOREIGN_PASSPORT,
    pii::RESIDENCE_PERMIT,
    pii::MILITARY_ID,
    pii::BIRTH_CERTIFICATE,
    pii::CARD_EXPIRY,
];

pub struct Masker<'a> {
    profile: &'a Profile,
    dict: &'a Dictionaries,
    secret: &'a [u8],
}

pub fn hmac_hex(secret: &[u8], ty: &str, key: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(ty.as_bytes());
    mac.update(b"\0");
    mac.update(key.as_bytes());
    let bytes = mac.finalize().into_bytes();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn patronymic_base(t: &str) -> String {
    const RULES: &[(&str, &str)] = &[
        ("овича", "ович"),
        ("овичу", "ович"),
        ("овичем", "ович"),
        ("овиче", "ович"),
        ("евича", "евич"),
        ("евичу", "евич"),
        ("евичем", "евич"),
        ("евиче", "евич"),
        ("овны", "овна"),
        ("овне", "овна"),
        ("овну", "овна"),
        ("овной", "овна"),
        ("евны", "евна"),
        ("евне", "евна"),
        ("евну", "евна"),
        ("евной", "евна"),
        ("ичны", "ична"),
        ("ичне", "ична"),
        ("ичну", "ична"),
        ("ичной", "ична"),
        ("ича", "ич"),
        ("ичу", "ич"),
        ("ичем", "ич"),
    ];
    for (suffix, base) in RULES {
        if let Some(stem) = t.strip_suffix(suffix) {
            return format!("{stem}{base}");
        }
    }
    t.to_string()
}

fn canonical_name_token(t: &str, dict: &Dictionaries) -> String {
    if let Some(nom) = dict.first_name_nominative.get(t) {
        return nom.clone();
    }
    if dict.is_patronymic(t) {
        return patronymic_base(t);
    }
    if let Some(lemma) = dict.surname_lemma(t) {
        return lemma;
    }
    inflect::surname_lemmas(t)
        .into_iter()
        .skip(1)
        .find(|l| inflect::surname_like(l))
        .unwrap_or_else(|| t.to_string())
}

/// Normalised identity of a value: two spellings of the same thing share it.
pub fn identity_key(ty: &PiiType, value: &str, dict: &Dictionaries) -> String {
    if NUMERIC_TYPES.contains(&ty.as_str()) {
        let d: String = validators::digits(value)
            .map(|x| char::from_digit(x, 10).unwrap_or('0'))
            .collect();
        if !d.is_empty() {
            if ty == pii::PHONE && d.len() == 11 && d.starts_with('8') {
                return format!("7{}", &d[1..]);
            }
            return d;
        }
    }
    let folded = fold(value);
    if ty == pii::FIO || ty == pii::CARD_HOLDER {
        let mut toks: Vec<String> = folded
            .split(|c: char| !c.is_alphanumeric() && c != '-')
            .filter(|t| !t.is_empty())
            .map(|t| canonical_name_token(t, dict))
            .collect();
        toks.sort();
        return toks.join(" ");
    }
    folded
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn maskable(c: char, mc: MaskChars) -> bool {
    match mc {
        MaskChars::Digits => c.is_ascii_digit(),
        MaskChars::Letters => c.is_alphabetic(),
        MaskChars::Alnum => c.is_alphanumeric(),
        MaskChars::All => !c.is_whitespace(),
    }
}

pub fn full(value: &str, fill: char, mc: MaskChars) -> String {
    let mc = if value.chars().any(|c| maskable(c, mc)) {
        mc
    } else {
        MaskChars::All
    };
    value
        .chars()
        .map(|c| if maskable(c, mc) { fill } else { c })
        .collect()
}

pub fn partial(
    value: &str,
    keep_start: usize,
    keep_end: usize,
    fill: char,
    mc: MaskChars,
) -> String {
    let mc = if value.chars().any(|c| maskable(c, mc)) {
        mc
    } else {
        MaskChars::All
    };
    let total = value.chars().filter(|c| maskable(*c, mc)).count();
    // Never leave more than half of the value visible.
    let (ks, ke) = if total <= keep_start + keep_end + 1 {
        (0, 0)
    } else {
        (keep_start, keep_end)
    };
    let mut seen = 0;
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if maskable(c, mc) {
            let keep = seen < ks || seen >= total - ke;
            out.push(if keep { c } else { fill });
            seen += 1;
        } else {
            out.push(c);
        }
    }
    out
}

pub fn initials(value: &str, with_dots: bool) -> String {
    let parts: Vec<String> = value
        .split(|c: char| c.is_whitespace() || c == '.')
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .filter_map(|w| w.chars().next().map(|c| c.to_uppercase().to_string()))
        .collect();
    if parts.is_empty() {
        return full(value, '*', MaskChars::All);
    }
    if with_dots {
        parts
            .iter()
            .map(|p| format!("{p}."))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        parts.join(" ")
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn pick<'a>(rng: &mut ChaCha8Rng, list: &'a [String]) -> &'a str {
    if list.is_empty() {
        return "";
    }
    &list[rng.gen_range(0..list.len())]
}

fn synthetic_patronymic(first: &str, female: bool) -> String {
    let (stem, suffix) = if let Some(stem) = first.strip_suffix('а') {
        (stem.to_string(), if female { "ична" } else { "ич" })
    } else if let Some(stem) = first.strip_suffix('й').or_else(|| first.strip_suffix('ь')) {
        (stem.to_string(), if female { "евна" } else { "евич" })
    } else {
        (first.to_string(), if female { "овна" } else { "ович" })
    };
    format!("{stem}{suffix}")
}

fn female_surname(s: &str) -> String {
    if let Some(stem) = s.strip_suffix("ский") {
        format!("{stem}ская")
    } else {
        format!("{s}а")
    }
}

fn synthetic_name(value: &str, rng: &mut ChaCha8Rng, dict: &Dictionaries) -> String {
    let folded = fold(value);
    let tokens: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|t| !t.is_empty())
        .collect();
    let female = tokens.iter().any(|t| {
        dict.female_names.contains(*t)
            || dict
                .first_name_nominative
                .get(*t)
                .map(|n| dict.female_names.contains(n))
                .unwrap_or(false)
            || t.ends_with("овна")
            || t.ends_with("евна")
            || t.ends_with("ична")
            || (t.ends_with("ова")
                || t.ends_with("ева")
                || t.ends_with("ина")
                || t.ends_with("ская"))
    });
    let first = pick(
        rng,
        if female {
            &dict.female_names_list
        } else {
            &dict.male_names_list
        },
    )
    .to_string();
    let base_first = if female {
        pick(rng, &dict.male_names_list).to_string()
    } else {
        first.clone()
    };
    let surname = {
        let s = pick(rng, &dict.surnames_list).to_string();
        if female {
            female_surname(&s)
        } else {
            s
        }
    };
    let patronymic = synthetic_patronymic(&base_first, female);
    let parts: Vec<String> = match tokens.len() {
        0 | 1 => vec![first],
        2 => vec![surname, first],
        _ => vec![surname, first, patronymic],
    };
    parts
        .iter()
        .map(|p| capitalize(p))
        .collect::<Vec<_>>()
        .join(" ")
}

fn luhn_complete(mut digits: Vec<u32>) -> Vec<u32> {
    digits.push(0);
    let len = digits.len();
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            if i % 2 == 1 {
                let y = x * 2;
                if y > 9 {
                    y - 9
                } else {
                    y
                }
            } else {
                x
            }
        })
        .sum();
    digits[len - 1] = (10 - sum % 10) % 10;
    digits
}

/// Replaces the digits of `value` positionally with `digits`, keeping every
/// non-digit character in place.
fn lay_out_digits(value: &str, digits: &[u32]) -> String {
    let mut it = digits.iter();
    value
        .chars()
        .map(|c| {
            if c.is_ascii_digit() {
                it.next()
                    .map(|d| char::from_digit(*d, 10).unwrap())
                    .unwrap_or('0')
            } else {
                c
            }
        })
        .collect()
}

fn random_digits(rng: &mut ChaCha8Rng, n: usize) -> Vec<u32> {
    (0..n).map(|_| rng.gen_range(0..10)).collect()
}

fn inn_digits(rng: &mut ChaCha8Rng, n: usize) -> Vec<u32> {
    let mut d = random_digits(rng, n);
    d[0] = rng.gen_range(1..10);
    if n == 10 {
        let w = [2, 4, 10, 3, 5, 9, 4, 6, 8];
        d[9] = w.iter().zip(&d).map(|(a, b)| a * b).sum::<u32>() % 11 % 10;
    } else {
        let w1 = [7, 2, 4, 10, 3, 5, 9, 4, 6, 8];
        d[10] = w1.iter().zip(&d).map(|(a, b)| a * b).sum::<u32>() % 11 % 10;
        let w2 = [3, 7, 2, 4, 10, 3, 5, 9, 4, 6, 8];
        d[11] = w2.iter().zip(&d).map(|(a, b)| a * b).sum::<u32>() % 11 % 10;
    }
    d
}

const STREETS: &[&str] = &[
    "Ленина",
    "Мира",
    "Советская",
    "Центральная",
    "Молодёжная",
    "Школьная",
    "Садовая",
    "Лесная",
    "Новая",
    "Набережная",
    "Заречная",
    "Полевая",
    "Зелёная",
    "Гагарина",
    "Пушкина",
    "Кирова",
    "Первомайская",
];
const MONTHS: &[&str] = &[
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
];
const COUNTRIES: &[&str] = &[
    "Российская Федерация",
    "Республика Беларусь",
    "Республика Казахстан",
    "Республика Армения",
];
const EMAIL_WORDS: &[&str] = &[
    "user", "client", "mail", "contact", "info", "person", "box", "account",
];

impl<'a> Masker<'a> {
    pub fn new(profile: &'a Profile, dict: &'a Dictionaries, secret: &'a [u8]) -> Self {
        Self {
            profile,
            dict,
            secret,
        }
    }

    fn synthetic(&self, ty: &PiiType, value: &str, key: &str) -> String {
        let hex = hmac_hex(self.secret, ty.as_str(), key);
        let seed = u64::from_str_radix(&hex[..16], 16).unwrap_or(0);
        let mut rng = ChaCha8Rng::seed_from_u64(seed);
        let dict = self.dict;
        let digit_count = validators::digit_count(value);
        match ty.as_str() {
            pii::FIO => synthetic_name(value, &mut rng, dict),
            pii::CARD_HOLDER => {
                let first = transliterate(pick(&mut rng, &dict.male_names_list));
                let last = transliterate(pick(&mut rng, &dict.surnames_list));
                format!("{} {}", first.to_uppercase(), last.to_uppercase())
            }
            pii::PHONE => {
                let mut d = vec![7, 9, rng.gen_range(0..10)];
                d.extend(random_digits(&mut rng, 8));
                if digit_count == 11 {
                    lay_out_digits(value, &d)
                } else {
                    format!(
                        "+7 9{}{} {}{}{} {}{} {}{}",
                        d[2], d[3], d[4], d[5], d[6], d[7], d[8], d[9], d[10]
                    )
                }
            }
            pii::EMAIL => format!(
                "{}{}@example.com",
                EMAIL_WORDS[rng.gen_range(0..EMAIL_WORDS.len())],
                rng.gen_range(100..999)
            ),
            pii::CARD_NUMBER => {
                let n = if (13..=19).contains(&digit_count) {
                    digit_count
                } else {
                    16
                };
                let mut d = vec![4];
                d.extend(random_digits(&mut rng, n - 2));
                let d = luhn_complete(d);
                if digit_count == n {
                    lay_out_digits(value, &d)
                } else {
                    d.chunks(4)
                        .map(|c| {
                            c.iter()
                                .map(|x| char::from_digit(*x, 10).unwrap())
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                }
            }
            pii::INN => {
                let n = if digit_count == 10 { 10 } else { 12 };
                lay_out_digits(value, &inn_digits(&mut rng, n))
            }
            pii::BIRTH_DATE | pii::PASSPORT_ISSUE_DATE | pii::DATE => {
                let (d, m, y) = (
                    rng.gen_range(1..=12),
                    rng.gen_range(1..=12),
                    rng.gen_range(1950..=2005),
                );
                if value.chars().any(|c| c.is_alphabetic()) {
                    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
                } else if digit_count == 8 {
                    let groups: Vec<&str> = value
                        .split(|c: char| !c.is_ascii_digit())
                        .filter(|g| !g.is_empty())
                        .collect();
                    let year_first = groups.first().map(|g| g.len() == 4).unwrap_or(false);
                    let digits: Vec<u32> = if year_first {
                        format!("{y:04}{m:02}{d:02}")
                    } else {
                        format!("{d:02}{m:02}{y:04}")
                    }
                    .chars()
                    .map(|c| c.to_digit(10).unwrap())
                    .collect();
                    lay_out_digits(value, &digits)
                } else {
                    format!("{d:02}.{m:02}.{y}")
                }
            }
            pii::ADDRESS => format!(
                "г. {}, ул. {}, д. {}, кв. {}",
                capitalize(pick(&mut rng, &dict.cities_list)),
                STREETS[rng.gen_range(0..STREETS.len())],
                rng.gen_range(1..120),
                rng.gen_range(1..300)
            ),
            pii::ADDRESS_CITY | pii::BIRTH_PLACE => {
                format!("г. {}", capitalize(pick(&mut rng, &dict.cities_list)))
            }
            pii::ADDRESS_STREET => format!("ул. {}", STREETS[rng.gen_range(0..STREETS.len())]),
            pii::ADDRESS_HOUSE => format!("д. {}", rng.gen_range(1..120)),
            pii::ADDRESS_APARTMENT => format!("кв. {}", rng.gen_range(1..300)),
            pii::ADDRESS_INDEX => format!("{}{:05}", rng.gen_range(1..7), rng.gen_range(0..100000)),
            pii::CITIZENSHIP | pii::ADDRESS_COUNTRY => {
                COUNTRIES[rng.gen_range(0..COUNTRIES.len())].to_string()
            }
            pii::PASSPORT_ISSUER => format!(
                "Отделом УФМС России по г. {}",
                capitalize(pick(&mut rng, &dict.cities_list))
            ),
            _ => {
                // Positional replacement: digits with digits, letters with letters.
                value
                    .chars()
                    .map(|c| {
                        if c.is_ascii_digit() {
                            char::from_digit(rng.gen_range(0..10), 10).unwrap()
                        } else if c.is_ascii_alphabetic() {
                            let base = if c.is_ascii_uppercase() { b'A' } else { b'a' };
                            (base + rng.gen_range(0..26u8)) as char
                        } else if ('а'..='я').contains(&c) {
                            char::from_u32('а' as u32 + rng.gen_range(0..32)).unwrap()
                        } else if ('А'..='Я').contains(&c) {
                            char::from_u32('А' as u32 + rng.gen_range(0..32)).unwrap()
                        } else {
                            c
                        }
                    })
                    .collect()
            }
        }
    }

    fn apply(
        &self,
        strategy: &StrategySpec,
        ty: &PiiType,
        value: &str,
        key: &str,
        ids: &mut HashMap<(String, String), usize>,
        counters: &mut HashMap<String, usize>,
    ) -> String {
        match strategy {
            StrategySpec::Initials { with_dots } => initials(value, *with_dots),
            StrategySpec::Partial {
                keep_start,
                keep_end,
                fill,
                mask_chars,
            } => partial(value, *keep_start, *keep_end, *fill, *mask_chars),
            StrategySpec::Full { fill, mask_chars } => full(value, *fill, *mask_chars),
            StrategySpec::Placeholder { template } => {
                let n = *ids
                    .entry((ty.to_string(), key.to_string()))
                    .or_insert_with(|| {
                        let c = counters.entry(ty.to_string()).or_insert(0);
                        *c += 1;
                        *c
                    });
                template
                    .replace("{type}", ty.as_str())
                    .replace("{TYPE}", ty.as_str())
                    .replace("{n}", &n.to_string())
            }
            StrategySpec::Synthetic => self.synthetic(ty, value, key),
            StrategySpec::Token { prefix, length } => {
                let h = hmac_hex(self.secret, ty.as_str(), key);
                format!("{prefix}{}", &h[..(*length).clamp(4, h.len())])
            }
            StrategySpec::Remove => String::new(),
        }
    }

    /// Replaces every entity in `text`; entities must be sorted and disjoint.
    pub fn mask(&self, text: &str, entities: &[Entity]) -> MaskResult {
        let mut state = MaskState::default();
        self.mask_with_state(text, entities, &mut state)
    }

    /// Like [`Masker::mask`], sharing placeholder identities across several
    /// texts (the messages of one chat request).
    pub fn mask_with_state(
        &self,
        text: &str,
        entities: &[Entity],
        state: &mut MaskState,
    ) -> MaskResult {
        let mut out = String::with_capacity(text.len());
        let mut entries = Vec::with_capacity(entities.len());
        let mut cursor = 0;
        for e in entities {
            if e.start < cursor || e.end > text.len() || e.start >= e.end {
                continue;
            }
            let value = &text[e.start..e.end];
            let strategy = self.profile.strategy_for(e.ty.as_str());
            let key = identity_key(&e.ty, value, self.dict);
            let mask = self.apply(
                strategy,
                &e.ty,
                value,
                &key,
                &mut state.ids,
                &mut state.counters,
            );
            out.push_str(&text[cursor..e.start]);
            let masked_start = out.len();
            out.push_str(&mask);
            let masked_end = out.len();
            entries.push(MaskEntry {
                ty: e.ty.clone(),
                start: e.start,
                end: e.end,
                masked_start,
                masked_end,
                mask,
                original: Sensitive::new(value.to_string()),
            });
            cursor = e.end;
        }
        out.push_str(&text[cursor..]);
        MaskResult {
            masked: out,
            entries,
            entities: entities.to_vec(),
        }
    }
}

/// Placeholder identities shared across texts.
#[derive(Default)]
pub struct MaskState {
    ids: HashMap<(String, String), usize>,
    counters: HashMap<String, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategies_match_reference_examples() {
        assert_eq!(initials("Иванов Иван Иванович", true), "И. И. И.");
        assert_eq!(initials("Иванов И.И.", true), "И. И. И.");
        assert_eq!(
            partial("4509 123456", 2, 2, '*', MaskChars::Digits),
            "45** ****56"
        );
        assert_eq!(
            partial("серия 4509 номер 123456", 2, 2, '*', MaskChars::Digits),
            "серия 45** номер ****56"
        );
        assert_eq!(partial("1234", 2, 2, '*', MaskChars::Digits), "****");
        assert_eq!(
            full("ivanov@mail.ru", '*', MaskChars::Alnum),
            "******@****.**"
        );
        assert_eq!(full("12.05.1987", '*', MaskChars::Digits), "**.**.****");
    }

    #[test]
    fn identity_unifies_spellings() {
        let dict = Dictionaries::builtin();
        let fio = PiiType::new(pii::FIO);
        assert_eq!(
            identity_key(&fio, "Иванов Иван Иванович", &dict),
            identity_key(&fio, "Иванова Ивана Ивановича", &dict)
        );
        assert_eq!(
            identity_key(&fio, "Иван Иванов", &dict),
            identity_key(&fio, "ИВАНОВ ИВАН", &dict)
        );
        let phone = PiiType::new(pii::PHONE);
        assert_eq!(
            identity_key(&phone, "+7 (925) 123-45-67", &dict),
            identity_key(&phone, "89251234567", &dict)
        );
    }
}
