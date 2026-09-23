//! Golden dataset: JSONL records with text, expected entities and the
//! expected masked text, plus a seeded generator producing synthetic
//! examples for every PII category, spelling variation and trap.

use maskarad_core::dict::transliterate;
use maskarad_core::engine::builtin_profiles;
use maskarad_core::mask::Masker;
use maskarad_core::types::pii;
use maskarad_core::{Dictionaries, Entity, PiiType};
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenEntity {
    #[serde(rename = "type")]
    pub ty: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GoldenRecord {
    pub id: String,
    pub category: String,
    pub text: String,
    pub entities: Vec<GoldenEntity>,
    /// Expected masked text under the `reference` profile.
    pub masked: String,
}

pub fn load(path: &Path) -> anyhow::Result<Vec<GoldenRecord>> {
    let mut out = Vec::new();
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();
    for file in files {
        let reader = std::io::BufReader::new(std::fs::File::open(&file)?);
        for (n, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let rec: GoldenRecord = serde_json::from_str(&line)
                .map_err(|e| anyhow::anyhow!("{}:{}: {e}", file.display(), n + 1))?;
            out.push(rec);
        }
    }
    Ok(out)
}

pub fn expected_mask(text: &str, entities: &[GoldenEntity], dict: &Dictionaries) -> String {
    let profiles = builtin_profiles();
    let profile = &profiles["reference"];
    let ents: Vec<Entity> = entities
        .iter()
        .map(|e| Entity {
            ty: PiiType::new(e.ty.clone()),
            start: e.start,
            end: e.end,
            confidence: 1.0,
            evidence: String::new(),
        })
        .collect();
    Masker::new(profile, dict, b"").mask(text, &ents).masked
}

// ---------------------------------------------------------------------------
// Generator
// ---------------------------------------------------------------------------

struct Builder {
    text: String,
    entities: Vec<GoldenEntity>,
}

impl Builder {
    fn new() -> Self {
        Self {
            text: String::new(),
            entities: Vec::new(),
        }
    }

    fn lit(&mut self, s: &str) -> &mut Self {
        self.text.push_str(s);
        self
    }

    fn ent(&mut self, ty: &str, value: &str) -> &mut Self {
        let start = self.text.len();
        self.text.push_str(value);
        self.entities.push(GoldenEntity {
            ty: ty.to_string(),
            start,
            end: self.text.len(),
        });
        self
    }

    fn finish(self, id: String, category: &str, dict: &Dictionaries) -> GoldenRecord {
        let masked = expected_mask(&self.text, &self.entities, dict);
        GoldenRecord {
            id,
            category: category.to_string(),
            text: self.text,
            entities: self.entities,
            masked,
        }
    }
}

pub struct Generator<'a> {
    rng: ChaCha8Rng,
    dict: &'a Dictionaries,
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn luhn_complete(mut d: Vec<u32>) -> Vec<u32> {
    d.push(0);
    let len = d.len();
    let sum: u32 = d
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
    d[len - 1] = (10 - sum % 10) % 10;
    d
}

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
    "Гагарина",
    "Пушкина",
    "Кирова",
    "Первомайская",
    "Тверская",
    "Большая Дмитровка",
    "8 Марта",
    "Профсоюзная",
];
const ISSUERS: &[&str] = &[
    "ОВД района Хамовники г. Москвы",
    "ГУ МВД России по г. Санкт-Петербургу",
    "Отделом УФМС России по Московской области в г. Мытищи",
    "ТП №5 ОУФМС России по Республике Татарстан",
    "МВД по Республике Татарстан",
    "Отделением по вопросам миграции ОМВД России по Кировскому району г. Казани",
    "УФМС России по Свердловской области",
    "ОУФМС России по г. Москве по району Митино",
];
const REGIONS: &[&str] = &[
    "Московская",
    "Ленинградская",
    "Свердловская",
    "Новосибирская",
    "Самарская",
    "Ростовская",
    "Нижегородская",
    "Челябинская",
];
const DOMAINS: &[&str] = &[
    "mail.ru",
    "gmail.com",
    "yandex.ru",
    "bk.ru",
    "inbox.ru",
    "rambler.ru",
];
const CITIZENSHIPS: &[&str] = &[
    "Российская Федерация",
    "РФ",
    "Россия",
    "Республика Беларусь",
    "Республика Казахстан",
    "Армения",
    "Узбекистан",
];

const NEGATIVES: &[&str] = &[
    "Поэт Александр Пушкин родился 6 июня 1799 года в Москве.",
    "Роман «Война и мир» написал Лев Николаевич Толстой.",
    "Композитор Пётр Ильич Чайковский родился в Воткинске.",
    "Отделение банка по адресу г. Москва, ул. Каланчёвская, д. 27 работает до 20:00.",
    "Горячая линия банка: 8 800 200-00-00, звонок бесплатный.",
    "Отчёт за 01.09.2026 готов, встреча назначена на 15.10.2026.",
    "В Москве и Санкт-Петербурге открылись новые офисы.",
    "Заказ № 123456 оформлен, сумма 1 500 000 руб., срок доставки 3 дня.",
    "Версия приложения 4.5.1, сборка 2026.",
    "Первый космонавт Юрий Гагарин совершил полёт 12 апреля 1961 года.",
    "Улица Пушкина и площадь Ленина находятся в центре города.",
    "Курс доллара составил 92,50 рубля, ставка 16%.",
    "Служба поддержки работает круглосуточно, телефон горячей линии 8 800 555-35-35.",
    "Памятник Михаилу Лермонтову стоит в Пятигорске.",
    "Пин-код нужно вводить на терминале, а CVV — на сайте магазина.",
    "Совещание перенесено на понедельник, повестка прежняя.",
    "Президент Владимир Путин выступил с обращением.",
    "Учёный Дмитрий Менделеев открыл периодический закон в 1869 году.",
    "Кредитная карта выпускается за 2 дня, лимит до 500 000 рублей.",
    "Рейс SU 1234 задерживается на 40 минут.",
];

impl<'a> Generator<'a> {
    pub fn new(seed: u64, dict: &'a Dictionaries) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(seed),
            dict,
        }
    }

    fn pick<'b>(&mut self, list: &'b [&str]) -> &'b str {
        list[self.rng.gen_range(0..list.len())]
    }

    fn pick_s<'b>(&mut self, list: &'b [String]) -> &'b str {
        &list[self.rng.gen_range(0..list.len())]
    }

    fn person(&mut self) -> (String, String, String, bool) {
        let female = self.rng.gen_bool(0.5);
        let dict = self.dict;
        let first = if female {
            self.pick_s(&dict.female_names_list)
        } else {
            self.pick_s(&dict.male_names_list)
        }
        .to_string();
        let base = self.pick_s(&dict.male_names_list).to_string();
        let surname = self.pick_s(&dict.surnames_list).to_string();
        let surname = if female {
            if let Some(stem) = surname.strip_suffix("ский") {
                format!("{stem}ская")
            } else {
                format!("{surname}а")
            }
        } else {
            surname
        };
        let patronymic = if let Some(stem) = base.strip_suffix('а') {
            format!("{stem}{}", if female { "ична" } else { "ич" })
        } else if let Some(stem) = base.strip_suffix('й').or_else(|| base.strip_suffix('ь')) {
            format!("{stem}{}", if female { "евна" } else { "евич" })
        } else {
            format!("{base}{}", if female { "овна" } else { "ович" })
        };
        (cap(&surname), cap(&first), cap(&patronymic), female)
    }

    fn fio(&mut self) -> String {
        let (s, f, p, _) = self.person();
        let form = self.rng.gen_range(0..6);
        let text = match form {
            0 | 1 => format!("{s} {f} {p}"),
            2 => format!("{f} {p} {s}"),
            3 => format!("{f} {s}"),
            4 => format!(
                "{s} {}.{}.",
                f.chars().next().unwrap(),
                p.chars().next().unwrap()
            ),
            _ => format!("{s} {f}"),
        };
        match self.rng.gen_range(0..8) {
            0 => text.to_lowercase(),
            1 => text.to_uppercase(),
            _ => text,
        }
    }

    fn digits(&mut self, n: usize) -> String {
        (0..n)
            .map(|_| char::from_digit(self.rng.gen_range(0..10), 10).unwrap())
            .collect()
    }

    fn phone(&mut self) -> String {
        let code = format!("9{:02}", self.rng.gen_range(0..100));
        let a = self.digits(3);
        let b = self.digits(2);
        let c = self.digits(2);
        match self.rng.gen_range(0..6) {
            0 => format!("+7 ({code}) {a}-{b}-{c}"),
            1 => format!("8 {code} {a} {b} {c}"),
            2 => format!("8{code}{a}{b}{c}"),
            3 => format!("+7-{code}-{a}-{b}-{c}"),
            4 => format!("+7 {code} {a} {b} {c}"),
            _ => format!("+7{code}{a}{b}{c}"),
        }
    }

    fn email(&mut self) -> String {
        let (s, f, _, _) = self.person();
        let domain = self.pick(DOMAINS);
        let local = match self.rng.gen_range(0..3) {
            0 => format!(
                "{}.{}",
                transliterate(&f.to_lowercase()),
                transliterate(&s.to_lowercase())
            ),
            1 => format!(
                "{}{}",
                transliterate(&s.to_lowercase()),
                self.rng.gen_range(1970..2005)
            ),
            _ => format!(
                "{}_{}",
                transliterate(&f.to_lowercase()),
                self.rng.gen_range(1..99)
            ),
        };
        let e = format!("{local}@{domain}");
        if self.rng.gen_bool(0.2) {
            cap(&e)
        } else {
            e
        }
    }

    fn inn(&mut self) -> String {
        let mut d: Vec<u32> = (0..12).map(|_| self.rng.gen_range(0..10)).collect();
        d[0] = self.rng.gen_range(1..10);
        // A third of the values are random digits without a valid checksum:
        // the reference dataset was most likely generated that way.
        if self.rng.gen_bool(0.3) {
            return d
                .iter()
                .map(|x| char::from_digit(*x, 10).unwrap())
                .collect();
        }
        let w1 = [7, 2, 4, 10, 3, 5, 9, 4, 6, 8];
        d[10] = w1.iter().zip(&d).map(|(a, b)| a * b).sum::<u32>() % 11 % 10;
        let w2 = [3, 7, 2, 4, 10, 3, 5, 9, 4, 6, 8];
        d[11] = w2.iter().zip(&d).map(|(a, b)| a * b).sum::<u32>() % 11 % 10;
        d.iter()
            .map(|x| char::from_digit(*x, 10).unwrap())
            .collect()
    }

    fn card(&mut self) -> String {
        let prefix: Vec<u32> = match self.rng.gen_range(0..3) {
            0 => vec![4],
            1 => vec![5, self.rng.gen_range(1..6)],
            _ => vec![2, 2, 0, self.rng.gen_range(0..5)],
        };
        let mut d = prefix;
        while d.len() < 15 {
            d.push(self.rng.gen_range(0..10));
        }
        let d = if self.rng.gen_bool(0.3) {
            d.push(self.rng.gen_range(0..10));
            d
        } else {
            luhn_complete(d)
        };
        let s: String = d
            .iter()
            .map(|x| char::from_digit(*x, 10).unwrap())
            .collect();
        match self.rng.gen_range(0..3) {
            0 => format!("{} {} {} {}", &s[0..4], &s[4..8], &s[8..12], &s[12..16]),
            1 => s,
            _ => format!("{}-{}-{}-{}", &s[0..4], &s[4..8], &s[8..12], &s[12..16]),
        }
    }

    fn date(&mut self) -> String {
        let d = self.rng.gen_range(1..=28);
        let m = self.rng.gen_range(1..=12);
        let y = self.rng.gen_range(1950..=2006);
        match self.rng.gen_range(0..8) {
            0 | 1 => format!("{d:02}.{m:02}.{y}"),
            2 => format!("{d:02}/{m:02}/{y}"),
            3 => format!("{y}-{m:02}-{d:02}"),
            4 => format!("{y}.{d:02}.{m:02}"),
            5 => format!("{d} {} {y}", MONTHS[m - 1]),
            6 => format!("{m:02}.{d:02}.{y}"),
            _ => format!("{} {d}, {y}", MONTHS[m - 1]),
        }
    }

    fn passport(&mut self) -> (String, String) {
        let series = format!(
            "{}{:02}",
            self.rng.gen_range(10..90),
            self.rng.gen_range(0..25)
        );
        let number = self.digits(6);
        match self.rng.gen_range(0..6) {
            0 => ("паспорт ".into(), format!("{series} {number}")),
            1 => (
                "паспорт РФ ".into(),
                format!("{} {} {number}", &series[..2], &series[2..]),
            ),
            2 => ("паспорт: ".into(), format!("{series}{number}")),
            3 => ("паспорт серия ".into(), format!("{series} номер {number}")),
            4 => (
                "паспорт серия ".into(),
                format!("{} {} № {number}", &series[..2], &series[2..]),
            ),
            _ => ("документ: паспорт ".into(), format!("{series}-{number}")),
        }
    }

    fn address(&mut self) -> String {
        let city = cap(self.pick_s(&self.dict.cities_list));
        let street = self.pick(STREETS).to_string();
        let house = self.rng.gen_range(1..150);
        let flat = self.rng.gen_range(1..400);
        let index = format!(
            "{}{:05}",
            self.rng.gen_range(1..7),
            self.rng.gen_range(0..100000)
        );
        let region = self.pick(REGIONS).to_string();
        match self.rng.gen_range(0..6) {
            0 => format!("{index}, г. {city}, ул. {street}, д. {house}, кв. {flat}"),
            1 => format!("г. {city}, {street} ул., д. {house}"),
            2 => format!(
                "{region} область, г. {city}, пр-т {street}, д. {house}, корп. {}, кв. {flat}",
                self.rng.gen_range(1..5)
            ),
            3 => format!("Россия, {index}, г. {city}, ул. {street}, дом {house}, квартира {flat}"),
            4 => format!("г. {city}, ул. {street}, д. {house}"),
            _ => format!("{index}, {region} обл., г. {city}, ул. {street}, д. {house}, кв. {flat}"),
        }
    }

    fn driver_license(&mut self) -> String {
        let r = self.rng.gen_range(10..99);
        const LETTERS: &[&str] = &["А", "В", "Е", "К", "М", "Н", "О", "Р", "С", "Т", "У", "Х"];
        let letters: String = (0..2).map(|_| self.pick(LETTERS).to_string()).collect();
        let n = self.digits(6);
        match self.rng.gen_range(0..4) {
            0 => format!("{r} {letters} {n}"),
            1 => format!("{r}{} {n}", self.digits(2)),
            2 => format!("{r} {} {n}", self.digits(2)),
            _ => format!("{r}{letters}{n}"),
        }
    }

    fn record(&mut self, category: &str, index: usize) -> GoldenRecord {
        let mut b = Builder::new();
        let dict = self.dict;
        match category {
            "FIO" => {
                let fio = self.fio();
                let (pre, post) = match self.rng.gen_range(0..5) {
                    0 => ("Клиент ", " обратился в отделение."),
                    1 => ("Заявление подал гражданин ", "."),
                    2 => ("Получатель платежа: ", ", назначение — возврат."),
                    3 => ("Договор заключён с ", " на 12 месяцев."),
                    _ => ("ФИО: ", "."),
                };
                b.lit(pre).ent(pii::FIO, &fio).lit(post);
            }
            "BIRTH_DATE" => {
                let fio = self.fio();
                let date = self.date();
                match self.rng.gen_range(0..4) {
                    0 => {
                        b.ent(pii::FIO, &fio)
                            .lit(", дата рождения ")
                            .ent(pii::BIRTH_DATE, &date)
                            .lit(".");
                    }
                    1 => {
                        b.lit("Клиент ")
                            .ent(pii::FIO, &fio)
                            .lit(" родился ")
                            .ent(pii::BIRTH_DATE, &date)
                            .lit(" года.");
                    }
                    2 => {
                        b.lit("Д.р.: ")
                            .ent(pii::BIRTH_DATE, &date)
                            .lit(", ФИО ")
                            .ent(pii::FIO, &fio)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Дата рождения заявителя — ")
                            .ent(pii::BIRTH_DATE, &date)
                            .lit(".");
                    }
                }
            }
            "BIRTH_PLACE" => {
                let city = cap(self.pick_s(&dict.cities_list));
                let fio = self.fio();
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.ent(pii::FIO, &fio)
                            .lit(", место рождения: ")
                            .ent(pii::BIRTH_PLACE, &format!("г. {city}"))
                            .lit(".");
                    }
                    1 => {
                        b.lit("Место рождения — ")
                            .ent(pii::BIRTH_PLACE, &format!("город {city}"))
                            .lit(", гражданин ")
                            .ent(pii::FIO, &fio)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Клиент ")
                            .ent(pii::FIO, &fio)
                            .lit(", уроженец ")
                            .ent(pii::BIRTH_PLACE, &format!("г. {city}"))
                            .lit(".");
                    }
                }
            }
            "PASSPORT" => {
                let (pre, value) = self.passport();
                let fio = self.fio();
                b.lit("Клиент ")
                    .ent(pii::FIO, &fio)
                    .lit(", ")
                    .lit(&pre)
                    .ent(pii::PASSPORT, &value)
                    .lit(".");
            }
            "CITIZENSHIP" => {
                let c = self.pick(CITIZENSHIPS).to_string();
                let fio = self.fio();
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.ent(pii::FIO, &fio)
                            .lit(", гражданство: ")
                            .ent(pii::CITIZENSHIP, &c)
                            .lit(".");
                    }
                    1 => {
                        b.lit("Гражданство — ")
                            .ent(pii::CITIZENSHIP, &c)
                            .lit(", заявитель ")
                            .ent(pii::FIO, &fio)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Клиент ")
                            .ent(pii::FIO, &fio)
                            .lit(" (гражданство ")
                            .ent(pii::CITIZENSHIP, &c)
                            .lit(").");
                    }
                }
            }
            "PASSPORT_ISSUER" => {
                let (pre, value) = self.passport();
                let issuer = self.pick(ISSUERS).to_string();
                let date = format!(
                    "{:02}.{:02}.{}",
                    self.rng.gen_range(1..29),
                    self.rng.gen_range(1..13),
                    self.rng.gen_range(2005..2024)
                );
                match self.rng.gen_range(0..2) {
                    0 => {
                        b.lit("Паспорт ")
                            .ent(pii::PASSPORT, &value)
                            .lit(", выдан ")
                            .ent(pii::PASSPORT_ISSUER, &issuer)
                            .lit(" ")
                            .ent(pii::PASSPORT_ISSUE_DATE, &date)
                            .lit(".");
                    }
                    _ => {
                        b.lit(&pre)
                            .ent(pii::PASSPORT, &value)
                            .lit("; кем выдан: ")
                            .ent(pii::PASSPORT_ISSUER, &issuer)
                            .lit(", дата выдачи ")
                            .ent(pii::PASSPORT_ISSUE_DATE, &date)
                            .lit(".");
                    }
                }
            }
            "SUBDIVISION_CODE" => {
                let (pre, value) = self.passport();
                let code = format!("{}-{}", self.digits(3), self.digits(3));
                match self.rng.gen_range(0..2) {
                    0 => {
                        b.lit(&pre)
                            .ent(pii::PASSPORT, &value)
                            .lit(", код подразделения ")
                            .ent(pii::SUBDIVISION_CODE, &code)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Код подразделения ")
                            .ent(pii::SUBDIVISION_CODE, &code)
                            .lit(", ")
                            .lit(&pre)
                            .ent(pii::PASSPORT, &value)
                            .lit(".");
                    }
                }
            }
            "PASSPORT_ISSUE_DATE" => {
                let (pre, value) = self.passport();
                let date = self.date();
                match self.rng.gen_range(0..2) {
                    0 => {
                        b.lit(&pre)
                            .ent(pii::PASSPORT, &value)
                            .lit(", дата выдачи ")
                            .ent(pii::PASSPORT_ISSUE_DATE, &date)
                            .lit(".");
                    }
                    _ => {
                        b.lit(&pre)
                            .ent(pii::PASSPORT, &value)
                            .lit(" выдан ")
                            .ent(pii::PASSPORT_ISSUE_DATE, &date)
                            .lit(".");
                    }
                }
            }
            "DRIVER_LICENSE" => {
                let dl = self.driver_license();
                let fio = self.fio();
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.lit("Водительское удостоверение ")
                            .ent(pii::DRIVER_LICENSE, &dl)
                            .lit(" на имя ")
                            .ent(pii::FIO, &fio)
                            .lit(".");
                    }
                    1 => {
                        b.ent(pii::FIO, &fio)
                            .lit(", в/у ")
                            .ent(pii::DRIVER_LICENSE, &dl)
                            .lit(".");
                    }
                    _ => {
                        b.lit("ВУ № ")
                            .ent(pii::DRIVER_LICENSE, &dl)
                            .lit(", владелец ")
                            .ent(pii::FIO, &fio)
                            .lit(".");
                    }
                }
            }
            "ADDRESS" => {
                let addr = self.address();
                let fio = self.fio();
                match self.rng.gen_range(0..4) {
                    0 => {
                        b.lit("Адрес: ").ent(pii::ADDRESS, &addr).lit(".");
                    }
                    1 => {
                        b.ent(pii::FIO, &fio)
                            .lit(" проживает по адресу ")
                            .ent(pii::ADDRESS, &addr)
                            .lit(".");
                    }
                    2 => {
                        b.lit("Зарегистрирован: ")
                            .ent(pii::ADDRESS, &addr)
                            .lit(", клиент ")
                            .ent(pii::FIO, &fio)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Доставка по адресу ")
                            .ent(pii::ADDRESS, &addr)
                            .lit(".");
                    }
                }
            }
            "EMAIL" => {
                let e = self.email();
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.lit("Email: ").ent(pii::EMAIL, &e).lit(".");
                    }
                    1 => {
                        b.lit("Свяжитесь со мной по почте ")
                            .ent(pii::EMAIL, &e)
                            .lit(" завтра.");
                    }
                    _ => {
                        let fio = self.fio();
                        b.ent(pii::FIO, &fio)
                            .lit(", e-mail ")
                            .ent(pii::EMAIL, &e)
                            .lit(".");
                    }
                }
            }
            "PHONE" => {
                let p = self.phone();
                match self.rng.gen_range(0..4) {
                    0 => {
                        b.lit("Телефон ").ent(pii::PHONE, &p).lit(".");
                    }
                    1 => {
                        b.lit("Перезвоните мне на ")
                            .ent(pii::PHONE, &p)
                            .lit(" после 18:00.");
                    }
                    2 => {
                        let fio = self.fio();
                        b.ent(pii::FIO, &fio)
                            .lit(", тел. ")
                            .ent(pii::PHONE, &p)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Контактный номер: ")
                            .ent(pii::PHONE, &p)
                            .lit(", email позже.");
                    }
                }
            }
            "INN" => {
                let inn = self.inn();
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.lit("ИНН ").ent(pii::INN, &inn).lit(".");
                    }
                    1 => {
                        let fio = self.fio();
                        b.ent(pii::FIO, &fio)
                            .lit(", ИНН: ")
                            .ent(pii::INN, &inn)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Налогоплательщик с ИНН ")
                            .ent(pii::INN, &inn)
                            .lit(" подал декларацию.");
                    }
                }
            }
            "CARD_NUMBER" => {
                let c = self.card();
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.lit("Карта ").ent(pii::CARD_NUMBER, &c).lit(".");
                    }
                    1 => {
                        b.lit("Перевод на карту ")
                            .ent(pii::CARD_NUMBER, &c)
                            .lit(" выполнен.");
                    }
                    _ => {
                        b.lit("Номер карты: ")
                            .ent(pii::CARD_NUMBER, &c)
                            .lit(", срок действия ")
                            .ent(pii::CARD_EXPIRY, "12/27")
                            .lit(".");
                    }
                }
            }
            "CVV" => {
                let c = self.card();
                let cvv = self.digits(3);
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.lit("Карта ")
                            .ent(pii::CARD_NUMBER, &c)
                            .lit(", CVV ")
                            .ent(pii::CVV, &cvv)
                            .lit(".");
                    }
                    1 => {
                        b.lit("CVC2: ")
                            .ent(pii::CVV, &cvv)
                            .lit(", карта ")
                            .ent(pii::CARD_NUMBER, &c)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Код безопасности ").ent(pii::CVV, &cvv).lit(".");
                    }
                }
            }
            "PIN" => {
                let pin = self.digits(4);
                match self.rng.gen_range(0..3) {
                    0 => {
                        let c = self.card();
                        b.lit("Карта ")
                            .ent(pii::CARD_NUMBER, &c)
                            .lit(", пин-код ")
                            .ent(pii::PIN, &pin)
                            .lit(".");
                    }
                    1 => {
                        b.lit("PIN: ").ent(pii::PIN, &pin).lit(".");
                    }
                    _ => {
                        b.lit("ПИН-код карты ")
                            .ent(pii::PIN, &pin)
                            .lit(" не сообщайте никому.");
                    }
                }
            }
            "CARD_HOLDER" => {
                let (s, f, _, _) = self.person();
                let holder = format!(
                    "{} {}",
                    transliterate(&f.to_lowercase()).to_uppercase(),
                    transliterate(&s.to_lowercase()).to_uppercase()
                );
                let c = self.card();
                match self.rng.gen_range(0..3) {
                    0 => {
                        b.lit("Карта ")
                            .ent(pii::CARD_NUMBER, &c)
                            .lit(", держатель ")
                            .ent(pii::CARD_HOLDER, &holder)
                            .lit(".");
                    }
                    1 => {
                        b.lit("Cardholder: ")
                            .ent(pii::CARD_HOLDER, &holder)
                            .lit(".");
                    }
                    _ => {
                        b.lit("Имя на карте ")
                            .ent(pii::CARD_HOLDER, &holder)
                            .lit(", номер ")
                            .ent(pii::CARD_NUMBER, &c)
                            .lit(".");
                    }
                }
            }
            "COMPLEX" => {
                let fio = self.fio();
                let date = self.date();
                let (pre, pass) = self.passport();
                let issuer = self.pick(ISSUERS).to_string();
                let code = format!("{}-{}", self.digits(3), self.digits(3));
                let addr = self.address();
                let phone = self.phone();
                let email = self.email();
                let inn = self.inn();
                let card = self.card();
                b.lit("Клиент ")
                    .ent(pii::FIO, &fio)
                    .lit(", дата рождения ")
                    .ent(pii::BIRTH_DATE, &date)
                    .lit(", ")
                    .lit(&pre)
                    .ent(pii::PASSPORT, &pass)
                    .lit(", выдан ")
                    .ent(pii::PASSPORT_ISSUER, &issuer)
                    .lit(", код подразделения ")
                    .ent(pii::SUBDIVISION_CODE, &code)
                    .lit(". Адрес регистрации: ")
                    .ent(pii::ADDRESS, &addr)
                    .lit(". Телефон ")
                    .ent(pii::PHONE, &phone)
                    .lit(", email ")
                    .ent(pii::EMAIL, &email)
                    .lit(", ИНН ")
                    .ent(pii::INN, &inn)
                    .lit(". Карта ")
                    .ent(pii::CARD_NUMBER, &card)
                    .lit(".");
            }
            "NEGATIVE" => {
                let s = NEGATIVES[index % NEGATIVES.len()];
                b.lit(s);
            }
            other => {
                b.lit(other);
            }
        }
        b.finish(
            format!("{}-{index:04}", category.to_lowercase()),
            category,
            dict,
        )
    }

    pub fn generate(&mut self, per_category: usize) -> Vec<GoldenRecord> {
        let categories = [
            "FIO",
            "BIRTH_DATE",
            "BIRTH_PLACE",
            "PASSPORT",
            "CITIZENSHIP",
            "PASSPORT_ISSUER",
            "SUBDIVISION_CODE",
            "PASSPORT_ISSUE_DATE",
            "DRIVER_LICENSE",
            "ADDRESS",
            "EMAIL",
            "PHONE",
            "INN",
            "CARD_NUMBER",
            "CVV",
            "PIN",
            "CARD_HOLDER",
            "COMPLEX",
            "NEGATIVE",
        ];
        let mut out = Vec::new();
        for cat in categories {
            let n = if cat == "NEGATIVE" {
                NEGATIVES.len().max(per_category.min(NEGATIVES.len()))
            } else {
                per_category
            };
            for i in 0..n {
                out.push(self.record(cat, i));
            }
        }
        out.shuffle(&mut self.rng);
        out
    }
}

/// Hand-written cases: the traps and spelling variations named in the task
/// statement, plus formats seen in real bank correspondence. Entities are
/// given as substrings and resolved to byte offsets.
type CuratedCase = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

const CURATED: &[CuratedCase] = &[
    ("contract", "Клиент Иванов Иван Иванович, паспорт 4509 123456", &[("FIO", "Иванов Иван Иванович"), ("PASSPORT", "4509 123456")]),
    ("case", "иванов иван иванович, паспорт 4509 123456", &[("FIO", "иванов иван иванович"), ("PASSPORT", "4509 123456")]),
    ("case", "ИВАНОВ ИВАН ИВАНОВИЧ, ПАСПОРТ 4509 123456", &[("FIO", "ИВАНОВ ИВАН ИВАНОВИЧ"), ("PASSPORT", "4509 123456")]),
    ("order", "Иван Иванович Иванов, тел. 8 (925) 123-45-67", &[("FIO", "Иван Иванович Иванов"), ("PHONE", "8 (925) 123-45-67")]),
    ("order", "Заявитель: Иванов И.И., ИНН 500100732259", &[("FIO", "Иванов И.И."), ("INN", "500100732259")]),
    ("order", "Получатель И. И. Иванов, счёт открыт", &[("FIO", "И. И. Иванов")]),
    ("date", "Дата рождения: 12.05.1987", &[("BIRTH_DATE", "12.05.1987")]),
    ("date", "Дата рождения: 05.12.1987 (мм.дд.гггг)", &[("BIRTH_DATE", "05.12.1987")]),
    ("date", "Дата рождения 1987.12.05", &[("BIRTH_DATE", "1987.12.05")]),
    ("date", "Родился 12 мая 1987 г. в г. Твери", &[("BIRTH_DATE", "12 мая 1987"), ("BIRTH_PLACE", "г. Твери")]),
    ("date", "родилась двенадцатого мая тысяча девятьсот восемьдесят седьмого года", &[("BIRTH_DATE", "двенадцатого мая тысяча девятьсот восемьдесят седьмого")]),
    ("date", "д.р. 1987-05-12, паспорт выдан 2010-06-15", &[("BIRTH_DATE", "1987-05-12"), ("PASSPORT_ISSUE_DATE", "2010-06-15")]),
    ("passport", "Паспорт серия 4509 номер 123456", &[("PASSPORT", "4509 номер 123456")]),
    ("passport", "паспорт: серия 45 09 № 123456, выдан ОВД района Хамовники г. Москвы 15.06.2010, код подразделения 770-001", &[("PASSPORT", "45 09 № 123456"), ("PASSPORT_ISSUER", "ОВД района Хамовники г. Москвы"), ("PASSPORT_ISSUE_DATE", "15.06.2010"), ("SUBDIVISION_CODE", "770-001")]),
    ("passport", "Паспорт 4509123456, к/п 770-001, гражданство РФ", &[("PASSPORT", "4509123456"), ("SUBDIVISION_CODE", "770-001"), ("CITIZENSHIP", "РФ")]),
    ("issuer", "выдан ГУ МВД России по г. Санкт-Петербургу и Ленинградской области 01.02.2019", &[("PASSPORT_ISSUER", "ГУ МВД России по г. Санкт-Петербургу и Ленинградской области"), ("PASSPORT_ISSUE_DATE", "01.02.2019")]),
    ("citizenship", "Гражданство: Российская Федерация", &[("CITIZENSHIP", "Российская Федерация")]),
    ("citizenship", "гражданин Республики Беларусь Петров Пётр Петрович", &[("CITIZENSHIP", "Республики Беларусь"), ("FIO", "Петров Пётр Петрович")]),
    ("dl", "Водительское удостоверение 77 АА 123456 выдано 22.03.2015", &[("DRIVER_LICENSE", "77 АА 123456"), ("PASSPORT_ISSUE_DATE", "22.03.2015")]),
    ("dl", "в/у 7712 123456", &[("DRIVER_LICENSE", "7712 123456")]),
    ("address", "Адрес: 101000, Россия, г. Москва, ул. Тверская, д. 7, корп. 2, кв. 12", &[("ADDRESS", "101000, Россия, г. Москва, ул. Тверская, д. 7, корп. 2, кв. 12")]),
    ("address", "Проживает: Московская обл., г. Мытищи, ул. Юбилейная, д. 5, кв. 17", &[("ADDRESS", "Московская обл., г. Мытищи, ул. Юбилейная, д. 5, кв. 17")]),
    ("address", "зарегистрирован по адресу г. Казань, Тверская ул., д. 7", &[("ADDRESS", "г. Казань, Тверская ул., д. 7")]),
    ("address", "Индекс 190000, Санкт-Петербург, Невский пр-т, д. 28", &[("ADDRESS", "190000, Санкт-Петербург, Невский пр-т, д. 28")]),
    ("email", "Почта: Ivanov.Ivan@Mail.RU или ivan_ivanov@почта.рф", &[("EMAIL", "Ivanov.Ivan@Mail.RU"), ("EMAIL", "ivan_ivanov@почта.рф")]),
    ("phone", "Телефоны: +7 925 123-45-67, 89251234567, +375 29 123 45 67", &[("PHONE", "+7 925 123-45-67"), ("PHONE", "89251234567"), ("PHONE", "+375 29 123 45 67")]),
    ("inn", "ИНН 500100732259 (физлицо), ИНН 7707083893 (организация)", &[("INN", "500100732259"), ("INN", "7707083893")]),
    // A number that fails Luhn is still a card when the text calls it one.
    ("card", "Карта 4276 3800 1234 5678, ещё 4111111111111111, CVV 123, пин-код 4321, держатель IVAN IVANOV, срок действия 12/27", &[("CARD_NUMBER", "4276 3800 1234 5678"), ("CARD_NUMBER", "4111111111111111"), ("CVV", "123"), ("PIN", "4321"), ("CARD_HOLDER", "IVAN IVANOV"), ("CARD_EXPIRY", "12/27")]),
    ("NEGATIVE", "Заказ 4276 3800 1234 5678 оформлен, трек-номер 123456789012", &[]),
    ("card", "cvv2: 321; card number 5500 0000 0000 0004; holder: Ivan Petrov", &[("CVV", "321"), ("CARD_NUMBER", "5500 0000 0000 0004"), ("CARD_HOLDER", "Ivan Petrov")]),
    ("documents", "СНИЛС 112-233-445 95, загранпаспорт 71 1234567, полис ОМС 1234567890123456", &[("SNILS", "112-233-445 95"), ("FOREIGN_PASSPORT", "71 1234567"), ("OMS_POLICY", "1234567890123456")]),
    ("multi", "Иванов Иван Иванович и Петрова Анна Сергеевна открыли счёт; Иванову звонить на +7 925 123-45-67", &[("FIO", "Иванов Иван Иванович"), ("FIO", "Петрова Анна Сергеевна"), ("FIO", "Иванову"), ("PHONE", "+7 925 123-45-67")]),
    ("NEGATIVE", "Поэт Александр Пушкин родился 6 июня 1799 года в Москве", &[]),
    ("NEGATIVE", "Лев Толстой написал «Войну и мир» в Ясной Поляне", &[]),
    ("NEGATIVE", "Отделение банка по адресу г. Москва, ул. Каланчёвская, д. 27 работает до 20:00", &[]),
    ("NEGATIVE", "Горячая линия 8 800 200-00-00, поддержка support@alfabank.ru", &[]),
    ("NEGATIVE", "Пин-код нужно ввести на терминале; CVV печатается на обороте карты", &[]),
    ("NEGATIVE", "Сумма 1 500 000 руб., срок 12 мес., ставка 16,5%, договор № 123456 от 01.09.2026", &[]),
    ("NEGATIVE", "В Москве открылся новый офис на улице Пушкина", &[]),
    ("NEGATIVE", "Заказ доставят 15.10.2026 курьером, время 20:00", &[]),
    ("NEGATIVE", "Улица Ленина, площадь Гагарина и станция метро Чеховская", &[]),
];

pub fn curated(dict: &Dictionaries) -> Vec<GoldenRecord> {
    let mut out = Vec::new();
    for (n, (category, text, ents)) in CURATED.iter().enumerate() {
        let mut entities = Vec::new();
        let mut search_from = 0;
        for (ty, value) in ents.iter() {
            let rel = text[search_from..]
                .find(value)
                .unwrap_or_else(|| panic!("curated case {n}: `{value}` not found"));
            let start = search_from + rel;
            entities.push(GoldenEntity {
                ty: ty.to_string(),
                start,
                end: start + value.len(),
            });
            search_from = start + value.len();
        }
        entities.sort_by_key(|e| e.start);
        let masked = expected_mask(text, &entities, dict);
        out.push(GoldenRecord {
            id: format!("curated-{n:03}"),
            category: category.to_string(),
            text: text.to_string(),
            entities,
            masked,
        });
    }
    out
}

pub fn write(path: &Path, records: &[GoldenRecord]) -> anyhow::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    for r in records {
        serde_json::to_writer(&mut f, r)?;
        f.write_all(b"\n")?;
    }
    Ok(())
}
