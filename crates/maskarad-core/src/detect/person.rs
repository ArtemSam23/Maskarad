//! Names of people: `Фамилия Имя Отчество` in any order and case, initials,
//! lone first names with context, Latin card-holder names. Public figures are
//! reported as `PUBLIC_PERSON` markers instead of `FIO`.

use super::context::{window_before, ContextMatcher};
use super::tokens::{Word, WordKind};
use super::{DetectCtx, Detector};
use crate::config::ContextSpec;
use crate::error::Result;
use crate::inflect;
use crate::types::{pii, Candidate};
use regex::Regex;

pub const PUBLIC_PERSON: &str = "PUBLIC_PERSON";

/// Common words that look like surnames by suffix or capitalisation but never
/// are one in the role of a name.
const NOT_A_SURNAME: &[&str] = &[
    "гражданин",
    "гражданка",
    "гражданину",
    "гражданке",
    "господин",
    "госпожа",
    "товарищ",
    "клиент",
    "клиентка",
    "клиенту",
    "заявитель",
    "заявительница",
    "получатель",
    "отправитель",
    "плательщик",
    "держатель",
    "сотрудник",
    "сотрудница",
    "заемщик",
    "заёмщик",
    "вкладчик",
    "владелец",
    "пациент",
    "абонент",
    "пользователь",
    "покупатель",
    "продавец",
    "арендатор",
    "доверитель",
    "представитель",
    "директор",
    "менеджер",
    "руководитель",
    "специалист",
    "магазин",
    "отделение",
    "компания",
    "банк",
    "договор",
    "заявление",
    "паспорт",
    "карта",
    "телефон",
    "адрес",
    "регион",
    "район",
    "город",
    "улица",
    "документ",
    "номер",
    "серия",
    "дата",
    "место",
    "время",
    "рублей",
    "копеек",
    "гражданство",
    "уроженец",
    "уроженка",
    "рождения",
    "выдан",
    "выдана",
    "кем",
    "код",
    "подразделения",
    "уважаемый",
    "уважаемая",
    "здравствуйте",
    "добрый",
    "день",
    "утро",
    "вечер",
    "спасибо",
    "пожалуйста",
    "композитор",
    "писатель",
    "поэт",
    "художник",
    "актер",
    "актёр",
    "режиссер",
    "режиссёр",
    "президент",
    "министр",
    "генерал",
    "профессор",
    "академик",
    "доктор",
    "учитель",
    "инженер",
    "водитель",
    "студент",
    "ученик",
    "клиентов",
];

/// Month names in the forms that appear in dates; never first names.
const MONTH_WORDS: &[&str] = &[
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
    "январь",
    "февраль",
    "март",
    "апрель",
    "май",
    "июнь",
    "июль",
    "август",
    "сентябрь",
    "октябрь",
    "ноябрь",
    "декабрь",
    "январе",
    "феврале",
    "марте",
    "апреле",
    "мае",
    "июне",
    "июле",
    "августе",
    "сентябре",
    "октябре",
    "ноябре",
    "декабре",
];

#[derive(Clone, Copy, Default)]
struct Role {
    first: bool,
    ambiguous: bool,
    patr: bool,
    surname_dict: bool,
    surname_like: bool,
    initial: bool,
    cap: bool,
    cyrillic: bool,
    len: usize,
}

impl Role {
    fn surname(&self) -> bool {
        self.surname_dict || self.surname_like
    }

    /// Unknown capitalised word that may still be a surname next to a first
    /// name and patronymic.
    fn surname_or_cap(&self) -> bool {
        self.surname() || (self.cap && self.cyrillic && !self.first && !self.patr && self.len >= 3)
    }
}

pub struct PersonDetector {
    lone_surname_cue: Regex,
    confidence: f32,
}

impl PersonDetector {
    pub fn new(confidence: f32) -> Result<Self> {
        let lone_surname_cue = Regex::new(
            r"(?:г-н|г-жа|господин|госпожа|фамилия|гражданин|гражданка|клиент|заявитель|сотрудник|заемщик|заёмщик)\s*[:\-]?\s*$",
        )
        .map_err(|source| crate::error::CoreError::Regex { ty: pii::FIO.into(), source })?;
        Ok(Self {
            lone_surname_cue,
            confidence,
        })
    }

    fn role(ctx: &DetectCtx<'_>, w: &Word, prev: Option<&Word>, next: Option<&Word>) -> Role {
        let lower = ctx.norm.lower.as_str();
        let t = w.text(lower);
        let digits = |x: Option<&Word>| x.map(|x| x.kind == WordKind::Digits).unwrap_or(false);
        // `5 мая 1990` is a date; `Мая Трубинова` is a name.
        let month_in_date = MONTH_WORDS.contains(&t) && (digits(prev) || digits(next));
        let len = t.chars().count();
        if w.kind != WordKind::Cyrillic {
            return Role {
                cap: w.capitalized,
                len,
                ..Default::default()
            };
        }
        let initial = len == 1 && lower[w.end..].starts_with('.');
        let stop = NOT_A_SURNAME.contains(&t);
        let first = ctx.dict.is_first_name(t) && !month_in_date && !stop;
        let ambiguous = first && ctx.dict.is_ambiguous_first_name(t);
        let patr = ctx.dict.is_patronymic(t);
        let (surname_dict, surname_like) = if stop {
            (false, false)
        } else if t.contains('-') {
            let parts: Vec<&str> = t.split('-').collect();
            (
                parts.iter().any(|p| ctx.dict.surname_lemma(p).is_some()),
                parts.iter().any(|p| inflect::surname_like(p)),
            )
        } else {
            (
                ctx.dict.surname_lemma(t).is_some(),
                inflect::surname_like(t),
            )
        };
        Role {
            first,
            ambiguous,
            patr,
            surname_dict,
            surname_like,
            initial,
            cap: w.capitalized && !stop,
            cyrillic: true,
            len,
        }
    }

    /// Words `a` and `b` are separated only by spaces (or a dot after an initial).
    fn adjacent(lower: &str, a: &Word, ra: &Role, b: &Word) -> bool {
        let gap = &lower[a.end..b.start];
        if gap.len() > 3 {
            return false;
        }
        gap.chars().all(|c| c == ' ' || (c == '.' && ra.initial))
    }

    fn match_at(
        &self,
        roles: &[Role],
        i: usize,
        run_end: usize,
        cued: bool,
    ) -> Option<(usize, f32, &'static str, bool)> {
        let r0 = roles[i];
        let r1 = (i + 1 < run_end).then(|| roles[i + 1]);
        let r2 = (i + 2 < run_end).then(|| roles[i + 2]);
        let s_conf = |r: Role| -> f32 {
            if r.surname_dict {
                0.97
            } else if r.surname_like {
                0.93
            } else {
                0.88
            }
        };
        if let (Some(r1), Some(r2)) = (r1, r2) {
            if r0.surname_or_cap() && !r0.patr && r1.first && r2.patr && !r1.patr {
                // `Клиент Иван Иванович Петров`: a capitalised non-surname word
                // before a name that carries its own surname after the patronymic.
                let r3 = (i + 3 < run_end).then(|| roles[i + 3]);
                let better_after = r3
                    .map(|r| r.surname() && !r.patr && !r.first)
                    .unwrap_or(false);
                if !r0.surname() && better_after {
                    return None;
                }
                return Some((3, s_conf(r0), "surname+first+patronymic", false));
            }
            if r0.first && r1.patr && r2.surname_or_cap() && !r2.patr && !r2.first {
                return Some((3, s_conf(r2), "first+patronymic+surname", false));
            }
            if r0.surname() && !r0.patr && r1.initial && r2.initial {
                return Some((3, 0.85, "surname+initials", false));
            }
            if r0.initial && r1.initial && r2.surname() && !r2.patr {
                return Some((3, 0.85, "initials+surname", false));
            }
        }
        if let Some(r1) = r1 {
            if r0.first && r1.patr && !r0.patr {
                return Some((2, 0.92, "first+patronymic", false));
            }
            // An ambiguous first name (`Вера`, `Марина`) needs capitalisation,
            // a dictionary surname next to it, or a cue word before the name.
            let ambiguous_ok = |f: Role, s: Role| !f.ambiguous || f.cap || s.surname_dict || cued;
            if r0.surname()
                && !r0.patr
                && !r0.initial
                && r1.first
                && !r1.patr
                && ambiguous_ok(r1, r0)
            {
                let conf = if r0.surname_dict { 0.85 } else { 0.78 };
                return Some((2, conf, "surname+first", false));
            }
            if r0.first
                && !r0.patr
                && r1.surname()
                && !r1.patr
                && !r1.initial
                && ambiguous_ok(r0, r1)
            {
                let conf = if r1.surname_dict { 0.85 } else { 0.78 };
                return Some((2, conf, "first+surname", false));
            }
            if r0.surname() && !r0.patr && r1.initial {
                return Some((2, 0.8, "surname+initial", false));
            }
            if r0.initial && r1.surname() && !r1.patr {
                return Some((2, 0.8, "initial+surname", false));
            }
        }
        if r0.first && !r0.patr && (!r0.ambiguous || r0.cap) {
            let conf = if r0.cap { 0.55 } else { 0.5 };
            return Some((1, conf, "first_name", true));
        }
        None
    }

    fn public_person(ctx: &DetectCtx<'_>, words: &[Word], roles: &[Role]) -> bool {
        let lower = ctx.norm.lower.as_str();
        let first_token = words
            .iter()
            .zip(roles)
            .find(|(_, r)| r.first && !r.patr)
            .map(|(w, _)| w.text(lower));
        for (w, r) in words.iter().zip(roles) {
            if r.patr || r.first || r.initial {
                continue;
            }
            for lemma in inflect::surname_lemmas(w.text(lower)) {
                let Some(person) = ctx.dict.public_person(&lemma) else {
                    continue;
                };
                match (&person.first_name, first_token) {
                    (Some(expected), Some(actual)) => {
                        if inflect::first_name_forms(expected)
                            .iter()
                            .any(|f| f == actual)
                        {
                            return true;
                        }
                    }
                    _ => return true,
                }
            }
        }
        false
    }
}

impl Detector for PersonDetector {
    fn name(&self) -> &'static str {
        "person"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        let words = ctx.words;
        let first_out = out.len();
        let roles: Vec<Role> = (0..words.len())
            .map(|i| {
                Self::role(
                    ctx,
                    &words[i],
                    i.checked_sub(1).map(|j| &words[j]),
                    words.get(i + 1),
                )
            })
            .collect();
        let n = words.len();
        let mut i = 0;
        while i < n {
            if !roles[i].cyrillic {
                i += 1;
                continue;
            }
            let mut run_end = i + 1;
            while run_end < n
                && roles[run_end].cyrillic
                && Self::adjacent(
                    lower,
                    &words[run_end - 1],
                    &roles[run_end - 1],
                    &words[run_end],
                )
            {
                run_end += 1;
            }
            let mut j = i;
            while j < run_end {
                let cued = self
                    .lone_surname_cue
                    .is_match(window_before(lower, words[j].start, 24));
                match self.match_at(&roles, j, run_end, cued) {
                    Some((len, conf, evidence, weak)) => {
                        let seq = &words[j..j + len];
                        let seq_roles = &roles[j..j + len];
                        let mut end = seq[len - 1].end;
                        if seq_roles[len - 1].initial && lower[end..].starts_with('.') {
                            end += 1;
                        }
                        let span = ctx.norm.to_orig_span(seq[0].start, end);
                        if len > 1 && Self::public_person(ctx, seq, seq_roles) {
                            out.push(
                                Candidate::new(span, PUBLIC_PERSON, 1.0, "person")
                                    .with_evidence("public_person"),
                            );
                        } else {
                            let conf = (conf * self.confidence / 0.9).min(1.0);
                            let mut c = Candidate::new(span, pii::FIO, conf, "person")
                                .with_evidence(evidence);
                            if weak {
                                c = c.weak();
                            }
                            out.push(c);
                        }
                        j += len;
                    }
                    None => {
                        let r = roles[j];
                        if r.surname() && !r.patr && r.cap {
                            let before = window_before(lower, words[j].start, 24);
                            if self.lone_surname_cue.is_match(before) {
                                let span = ctx.norm.to_orig_span(words[j].start, words[j].end);
                                out.push(
                                    Candidate::new(span, pii::FIO, 0.7, "person")
                                        .with_evidence("surname+cue"),
                                );
                            }
                        }
                        j += 1;
                    }
                }
            }
            i = run_end;
        }

        // Co-reference: `Иванов Иван Иванович … Иванову перезвонить` — a lone
        // surname of a person already found in the text is the same person.
        let mut known: Vec<String> = Vec::new();
        let mut covered: Vec<(usize, usize)> = Vec::new();
        for c in &out[first_out..] {
            if c.ty != pii::FIO || c.weak {
                continue;
            }
            covered.push((c.span.start, c.span.end));
            let ls = ctx.norm.lower_offset(c.span.start);
            let le = ctx.norm.lower_offset(c.span.end);
            for tok in lower[ls..le].split(|ch: char| !ch.is_alphanumeric() && ch != '-') {
                if tok.chars().count() >= 3
                    && !ctx.dict.is_first_name(tok)
                    && !ctx.dict.is_patronymic(tok)
                {
                    if let Some(lemma) = inflect::surname_lemmas(tok)
                        .into_iter()
                        .find(|l| ctx.dict.surnames.contains(l) || inflect::surname_like(l))
                    {
                        known.push(lemma);
                    }
                }
            }
        }
        covered.sort_unstable();
        if !known.is_empty() {
            for (w, r) in words.iter().zip(&roles) {
                if !r.cyrillic || r.first || r.patr || !r.cap || r.len < 3 {
                    continue;
                }
                let span = ctx.norm.to_orig_span(w.start, w.end);
                let idx = covered.partition_point(|(s, _)| *s < span.end);
                if idx > 0 && covered[idx - 1].1 > span.start {
                    continue;
                }
                if inflect::surname_lemmas(w.text(lower))
                    .iter()
                    .any(|l| known.contains(l))
                {
                    out.push(
                        Candidate::new(span, pii::FIO, 0.85, "person")
                            .with_evidence("surname+coreference"),
                    );
                }
            }
        }
    }
}

/// Latin names on payment cards (`IVAN IVANOV`).
pub struct CardHolderDetector {
    ctx: ContextMatcher,
}

const CARD_WORD_STOPLIST: &[&str] = &[
    "visa",
    "mastercard",
    "maestro",
    "mir",
    "classic",
    "gold",
    "platinum",
    "black",
    "edition",
    "online",
    "bank",
    "card",
    "credit",
    "debit",
    "unionpay",
    "american",
    "express",
    "world",
    "elite",
    "signature",
    "infinite",
    "business",
    "premium",
    "standard",
    "electron",
    "virtual",
    "digital",
    "apple",
    "google",
    "pay",
    "samsung",
    "sber",
    "tinkoff",
    "alfa",
    "vtb",
    "raiffeisen",
    "rosbank",
    "otkritie",
    "sovcombank",
    "gazprombank",
    "cvv",
    "cvc",
    "pin",
    "exp",
    "valid",
    "thru",
    "month",
    "year",
    "id",
    "code",
    "no",
    "number",
    "the",
    "and",
    "for",
    "with",
    "from",
    "llc",
    "ltd",
    "inc",
    "ooo",
    "pao",
    "ao",
    "ip",
];

impl CardHolderDetector {
    pub fn new(spec: &ContextSpec, default_window: usize) -> Result<Self> {
        Ok(Self {
            ctx: ContextMatcher::compile(spec, default_window, pii::CARD_HOLDER)?,
        })
    }
}

impl Detector for CardHolderDetector {
    fn name(&self) -> &'static str {
        "card_holder"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        let words = ctx.words;
        let n = words.len();
        let mut i = 0;
        while i < n {
            let usable = |w: &Word| {
                w.kind == WordKind::Latin
                    && w.end - w.start >= 2
                    && !CARD_WORD_STOPLIST.contains(&w.text(lower))
            };
            if !usable(&words[i]) {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < n
                && j - i < 3
                && usable(&words[j])
                && lower[words[j - 1].end..words[j].start]
                    .chars()
                    .all(|c| c == ' ')
                && words[j].start - words[j - 1].end <= 2
            {
                j += 1;
            }
            if j - i >= 2 {
                let seq = &words[i..j];
                let name_hit = seq
                    .iter()
                    .any(|w| ctx.dict.first_names_latin.contains(w.text(lower)));
                let cased = seq.iter().all(|w| w.capitalized || w.all_caps);
                let cr = self.ctx.check(lower, seq[0].start, seq[j - i - 1].end);
                if name_hit || cased || cr.matched {
                    let span = ctx.norm.to_orig_span(seq[0].start, seq[j - i - 1].end);
                    let conf = match (name_hit, cr.matched) {
                        (true, true) => 0.95,
                        (true, false) => 0.85,
                        (false, true) => 0.8,
                        (false, false) => 0.55,
                    };
                    let mut c = Candidate::new(span, pii::CARD_HOLDER, conf, "card_holder")
                        .with_evidence(if name_hit {
                            "latin_name"
                        } else {
                            "latin_words"
                        });
                    if !name_hit && !cr.matched {
                        c = c.weak();
                    }
                    out.push(c);
                }
            }
            i = j.max(i + 1);
        }
    }
}
