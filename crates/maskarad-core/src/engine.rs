//! The engine: everything compiled from a [`CoreConfig`] — dictionaries,
//! detectors, resolver, profiles and consumer systems — and the three
//! operations: detect, mask, demask.

use crate::config::{CoreConfig, Profile, StrategySpec, SystemDef, TypeKind};
use crate::demask;
use crate::detect::address::AddressDetector;
use crate::detect::dates::{DateConfig, DateDetector};
use crate::detect::gazetteer::Gazetteer;
use crate::detect::issuer::IssuerDetector;
use crate::detect::pattern::PatternDetector;
use crate::detect::person::{CardHolderDetector, PersonDetector};
use crate::detect::place::{BirthPlaceDetector, CitizenshipDetector};
use crate::detect::tokens::tokenize;
use crate::detect::{DetectCtx, Detector};
use crate::dict::Dictionaries;
use crate::error::{CoreError, Result};
use crate::mask::{MaskState, Masker};
use crate::normalize::Normalized;
use crate::registry::TypeRegistry;
use crate::resolve::{Policy, Resolver};
use crate::types::{pii, Candidate, Entity, MaskEntry, MaskResult};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A consumer system with its policy and masking profile resolved.
#[derive(Clone, Debug)]
pub struct CompiledSystem {
    pub def: SystemDef,
    pub policy: Policy,
    pub profile: Profile,
    /// SHA-256 hex digests of the accepted API keys.
    pub key_hashes: Vec<String>,
}

impl CompiledSystem {
    pub fn id(&self) -> &str {
        &self.def.id
    }

    pub fn accepts_key(&self, key: &str) -> bool {
        let digest = sha256_hex(key);
        self.key_hashes.contains(&digest)
    }
}

pub fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Profiles shipped with the engine.
pub fn builtin_profiles() -> HashMap<String, Profile> {
    let mut profiles = HashMap::new();
    let mut reference = Profile {
        description: "Эталон автопроверки: инициалы для имён, частичная маска для номеров, полная для остального".into(),
        default: StrategySpec::full(),
        types: HashMap::new(),
    };
    for ty in [pii::FIO, pii::CARD_HOLDER] {
        reference.types.insert(ty.into(), StrategySpec::initials());
    }
    for ty in [
        pii::PASSPORT,
        pii::CARD_NUMBER,
        pii::PHONE,
        pii::INN,
        pii::SNILS,
        pii::DRIVER_LICENSE,
        pii::SUBDIVISION_CODE,
        pii::BANK_ACCOUNT,
        pii::OMS_POLICY,
        pii::FOREIGN_PASSPORT,
        pii::RESIDENCE_PERMIT,
        pii::MILITARY_ID,
        pii::BIRTH_CERTIFICATE,
    ] {
        reference
            .types
            .insert(ty.into(), StrategySpec::partial_digits(2, 2));
    }
    for ty in [
        pii::BIRTH_DATE,
        pii::PASSPORT_ISSUE_DATE,
        pii::DATE,
        pii::CARD_EXPIRY,
        pii::CVV,
        pii::PIN,
    ] {
        // Digits and month words alike: `12 мая 1987` → `** *** ****`.
        reference.types.insert(ty.into(), StrategySpec::full());
    }
    profiles.insert("reference".into(), reference);
    profiles.insert(
        "placeholder".into(),
        Profile {
            description: "Уникальные плейсхолдеры <TYPE_n> для LLM-сценариев".into(),
            default: StrategySpec::placeholder(),
            types: HashMap::new(),
        },
    );
    profiles.insert(
        "synthetic".into(),
        Profile {
            description: "Замена правдоподобными синтетическими значениями".into(),
            default: StrategySpec::Synthetic,
            types: HashMap::new(),
        },
    );
    profiles.insert(
        "token".into(),
        Profile {
            description: "Необратимые стабильные токены (HMAC)".into(),
            default: StrategySpec::token(),
            types: HashMap::new(),
        },
    );
    profiles.insert(
        "full".into(),
        Profile {
            description: "Полная позиционная маска звёздочками".into(),
            default: StrategySpec::full(),
            types: HashMap::new(),
        },
    );
    profiles
}

pub struct Engine {
    dict: Dictionaries,
    gazetteer: Gazetteer,
    detectors: Vec<Box<dyn Detector>>,
    resolver: Resolver,
    systems: HashMap<String, CompiledSystem>,
    anonymous: Option<String>,
    profiles: HashMap<String, Profile>,
    window: usize,
    secret: Vec<u8>,
}

impl Engine {
    pub fn new(cfg: &CoreConfig) -> Result<Self> {
        let registry = TypeRegistry::with_overrides(&cfg.pii_types)?;
        let dict = Dictionaries::from_config(&cfg.dictionaries, &cfg.allowlists.public_persons)?;
        let gazetteer = Gazetteer::build(&dict)?;
        let window = cfg.detection.context_window;

        let mut detectors: Vec<Box<dyn Detector>> = Vec::new();
        let pattern = PatternDetector::compile(registry.defs(), window)?;
        if pattern.type_count() > 0 {
            detectors.push(Box::new(pattern));
        }
        let enabled = |id: &str| {
            registry
                .get(id)
                .map(|d| d.enabled && d.kind == TypeKind::Builtin)
                .unwrap_or(false)
        };
        let cues = |id: &str| {
            registry
                .get(id)
                .map(|d| d.context.before.clone())
                .unwrap_or_default()
        };
        let conf = |id: &str| registry.get(id).map(|d| d.confidence).unwrap_or(0.9);

        if enabled(pii::BIRTH_DATE) || enabled(pii::PASSPORT_ISSUE_DATE) || enabled(pii::DATE) {
            let defaults = DateConfig::default();
            let birth = registry.get(pii::BIRTH_DATE);
            let issue = registry.get(pii::PASSPORT_ISSUE_DATE);
            let date_cfg = DateConfig {
                birth_before: birth
                    .filter(|d| !d.context.before.is_empty())
                    .map(|d| d.context.before.clone())
                    .unwrap_or(defaults.birth_before),
                birth_after: birth
                    .filter(|d| !d.context.after.is_empty())
                    .map(|d| d.context.after.clone())
                    .unwrap_or(defaults.birth_after),
                issue_before: issue
                    .filter(|d| !d.context.before.is_empty())
                    .map(|d| d.context.before.clone())
                    .unwrap_or(defaults.issue_before),
                window: birth
                    .map(|d| d.context.window)
                    .filter(|w| *w > 0)
                    .unwrap_or(defaults.window),
                birth_confidence: conf(pii::BIRTH_DATE),
                issue_confidence: conf(pii::PASSPORT_ISSUE_DATE),
                plain_confidence: conf(pii::DATE),
                birth_enabled: enabled(pii::BIRTH_DATE),
                issue_enabled: enabled(pii::PASSPORT_ISSUE_DATE),
                plain_enabled: enabled(pii::DATE),
            };
            detectors.push(Box::new(DateDetector::new(date_cfg)?));
        }
        if enabled(pii::FIO) {
            detectors.push(Box::new(PersonDetector::new(conf(pii::FIO))?));
        }
        if enabled(pii::CARD_HOLDER) {
            let spec = registry
                .get(pii::CARD_HOLDER)
                .map(|d| d.context.clone())
                .unwrap_or_default();
            detectors.push(Box::new(CardHolderDetector::new(&spec, window)?));
        }
        if enabled(pii::BIRTH_PLACE) {
            if let Some(d) =
                BirthPlaceDetector::new(&cues(pii::BIRTH_PLACE), conf(pii::BIRTH_PLACE))?
            {
                detectors.push(Box::new(d));
            }
        }
        if enabled(pii::CITIZENSHIP) {
            if let Some(d) =
                CitizenshipDetector::new(&cues(pii::CITIZENSHIP), conf(pii::CITIZENSHIP))?
            {
                detectors.push(Box::new(d));
            }
        }
        if enabled(pii::PASSPORT_ISSUER) {
            detectors.push(Box::new(IssuerDetector::new(
                &cues(pii::PASSPORT_ISSUER),
                conf(pii::PASSPORT_ISSUER),
            )?));
        }
        if enabled(pii::ADDRESS) {
            let spec = registry
                .get(pii::ADDRESS)
                .map(|d| d.context.clone())
                .unwrap_or_default();
            detectors.push(Box::new(AddressDetector::new(
                &spec,
                window,
                conf(pii::ADDRESS),
            )?));
        }

        let mut profiles = builtin_profiles();
        for (name, p) in &cfg.profiles {
            profiles.insert(name.clone(), p.clone());
        }

        let mut systems = HashMap::new();
        let mut anonymous = None;
        for def in &cfg.systems {
            let base = profiles
                .get(&def.profile)
                .ok_or_else(|| CoreError::UnknownProfile(def.profile.clone()))?;
            let mut profile = base.clone();
            for (ty, s) in &def.overrides {
                profile.types.insert(ty.clone(), s.clone());
            }
            let key_hashes = def
                .auth
                .api_keys
                .iter()
                .map(|k| match k.strip_prefix("sha256:") {
                    Some(hex) => hex.to_ascii_lowercase(),
                    None => sha256_hex(k),
                })
                .collect();
            if def.auth.anonymous && def.enabled {
                if anonymous.is_some() {
                    return Err(CoreError::Config("only one system may be anonymous".into()));
                }
                anonymous = Some(def.id.clone());
            }
            if systems.contains_key(&def.id) {
                return Err(CoreError::Config(format!(
                    "duplicate system id `{}`",
                    def.id
                )));
            }
            systems.insert(
                def.id.clone(),
                CompiledSystem {
                    policy: Policy::new(def, &registry, cfg.detection.min_confidence),
                    profile,
                    key_hashes,
                    def: def.clone(),
                },
            );
        }

        let resolver = Resolver::new(registry, &cfg.allowlists, window)?;
        Ok(Self {
            dict,
            gazetteer,
            detectors,
            resolver,
            systems,
            anonymous,
            profiles,
            window,
            secret: cfg.masking.secret.clone().unwrap_or_default().into_bytes(),
        })
    }

    pub fn system(&self, id: &str) -> Option<&CompiledSystem> {
        self.systems.get(id)
    }

    pub fn systems(&self) -> impl Iterator<Item = &CompiledSystem> {
        self.systems.values()
    }

    /// The system that unauthenticated requests are attributed to, if any.
    pub fn anonymous_system(&self) -> Option<&CompiledSystem> {
        self.anonymous
            .as_deref()
            .and_then(|id| self.systems.get(id))
    }

    /// System owning the given API key.
    pub fn system_by_key(&self, key: &str) -> Option<&CompiledSystem> {
        self.systems.values().find(|s| s.accepts_key(key))
    }

    pub fn profiles(&self) -> &HashMap<String, Profile> {
        &self.profiles
    }

    pub fn registry(&self) -> &TypeRegistry {
        self.resolver.registry()
    }

    pub fn dictionaries(&self) -> &Dictionaries {
        &self.dict
    }

    /// Raw candidates of every detector (for diagnostics).
    pub fn candidates(&self, text: &str) -> Vec<Candidate> {
        let norm = Normalized::new(text);
        let words = tokenize(text, &norm);
        let places = self.gazetteer.find(&norm.lower);
        let ctx = DetectCtx {
            text,
            norm: &norm,
            words: &words,
            dict: &self.dict,
            places: &places,
            window: self.window,
        };
        let mut out = Vec::new();
        for d in &self.detectors {
            d.detect(&ctx, &mut out);
        }
        out
    }

    /// Wall time and candidate count of every detector on `text` (diagnostics).
    pub fn detector_timings(&self, text: &str) -> Vec<(&'static str, std::time::Duration, usize)> {
        let norm = Normalized::new(text);
        let words = tokenize(text, &norm);
        let places = self.gazetteer.find(&norm.lower);
        let ctx = DetectCtx {
            text,
            norm: &norm,
            words: &words,
            dict: &self.dict,
            places: &places,
            window: self.window,
        };
        self.detectors
            .iter()
            .map(|d| {
                let started = std::time::Instant::now();
                let mut out = Vec::new();
                d.detect(&ctx, &mut out);
                (d.name(), started.elapsed(), out.len())
            })
            .collect()
    }

    /// Entities accepted for the system.
    pub fn detect(&self, text: &str, system: &CompiledSystem) -> Vec<Entity> {
        let norm = Normalized::new(text);
        let words = tokenize(text, &norm);
        let places = self.gazetteer.find(&norm.lower);
        let ctx = DetectCtx {
            text,
            norm: &norm,
            words: &words,
            dict: &self.dict,
            places: &places,
            window: self.window,
        };
        let mut candidates = Vec::new();
        for d in &self.detectors {
            d.detect(&ctx, &mut candidates);
        }
        self.resolver
            .resolve(text, &norm, candidates, &system.policy)
    }

    pub fn mask(&self, text: &str, system: &CompiledSystem) -> MaskResult {
        let entities = self.detect(text, system);
        Masker::new(&system.profile, &self.dict, &self.secret).mask(text, &entities)
    }

    /// Mask sharing placeholder identities across texts (chat messages).
    pub fn mask_with_state(
        &self,
        text: &str,
        system: &CompiledSystem,
        state: &mut MaskState,
    ) -> MaskResult {
        let entities = self.detect(text, system);
        Masker::new(&system.profile, &self.dict, &self.secret)
            .mask_with_state(text, &entities, state)
    }

    /// Rebuild the masked text from the original and recorded replacements.
    pub fn rebuild_masked(text: &str, entries: &[MaskEntry]) -> Option<String> {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for e in entries {
            if e.start < cursor
                || e.end > text.len()
                || !text.is_char_boundary(e.start)
                || !text.is_char_boundary(e.end)
            {
                return None;
            }
            out.push_str(&text[cursor..e.start]);
            out.push_str(&e.mask);
            cursor = e.end;
        }
        out.push_str(&text[cursor..]);
        Some(out)
    }

    /// Restore originals. `exact` means the input is the masked text itself.
    pub fn demask(&self, text: &str, entries: &[MaskEntry], exact: bool) -> String {
        if exact {
            if let Some(out) = demask::demask_exact(text, entries) {
                return out;
            }
        }
        demask::demask_substitute(text, entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        let cfg: CoreConfig = serde_yaml::from_str(
            r#"
systems:
  - id: test
    auth: { anonymous: true }
  - id: llm
    profile: placeholder
    auth: { api_keys: ["secret-key"] }
"#,
        )
        .unwrap();
        Engine::new(&cfg).unwrap()
    }

    fn types(engine: &Engine, text: &str) -> Vec<(String, String)> {
        let sys = engine.system("test").unwrap();
        engine
            .detect(text, sys)
            .into_iter()
            .map(|e| (e.ty.to_string(), text[e.start..e.end].to_string()))
            .collect()
    }

    #[test]
    fn contract_example() {
        let e = engine();
        let sys = e.system("test").unwrap();
        let r = e.mask("Клиент Иванов Иван Иванович, паспорт 4509 123456", sys);
        assert_eq!(r.masked, "Клиент И. И. И., паспорт 45** ****56");
        assert_eq!(
            e.demask(&r.masked, &r.entries, true),
            "Клиент Иванов Иван Иванович, паспорт 4509 123456"
        );
    }

    #[test]
    fn detects_all_core_types() {
        let e = engine();
        let text = "Иванов Иван Иванович, дата рождения 12.05.1987, место рождения: г. Москва, гражданство: Российская Федерация. \
Паспорт серия 45 09 номер 123456, выдан ОВД района Хамовники г. Москвы 15.06.2010, код подразделения 770-001. \
Водительское удостоверение 77 АА 123456. Адрес: 101000, г. Москва, ул. Тверская, д. 7, кв. 12. \
Email: Ivanov.Ivan@Mail.ru, телефон +7 (925) 123-45-67, ИНН 500100732259. \
Карта 4111 1111 1111 1111, CVV 123, пин-код 4321, держатель IVAN IVANOV.";
        let found = types(&e, text);
        let has = |ty: &str, v: &str| found.iter().any(|(t, s)| t == ty && s == v);
        assert!(has("FIO", "Иванов Иван Иванович"), "{found:?}");
        assert!(has("BIRTH_DATE", "12.05.1987"), "{found:?}");
        assert!(has("BIRTH_PLACE", "г. Москва"), "{found:?}");
        assert!(has("CITIZENSHIP", "Российская Федерация"), "{found:?}");
        assert!(has("PASSPORT", "45 09 номер 123456"), "{found:?}");
        assert!(
            has("PASSPORT_ISSUER", "ОВД района Хамовники г. Москвы"),
            "{found:?}"
        );
        assert!(has("PASSPORT_ISSUE_DATE", "15.06.2010"), "{found:?}");
        assert!(has("SUBDIVISION_CODE", "770-001"), "{found:?}");
        assert!(has("DRIVER_LICENSE", "77 АА 123456"), "{found:?}");
        assert!(
            has("ADDRESS", "101000, г. Москва, ул. Тверская, д. 7, кв. 12"),
            "{found:?}"
        );
        assert!(has("EMAIL", "Ivanov.Ivan@Mail.ru"), "{found:?}");
        assert!(has("PHONE", "+7 (925) 123-45-67"), "{found:?}");
        assert!(has("INN", "500100732259"), "{found:?}");
        assert!(has("CARD_NUMBER", "4111 1111 1111 1111"), "{found:?}");
        assert!(has("CVV", "123"), "{found:?}");
        assert!(has("PIN", "4321"), "{found:?}");
        assert!(has("CARD_HOLDER", "IVAN IVANOV"), "{found:?}");
    }

    #[test]
    fn case_insensitive_and_date_variants() {
        let e = engine();
        let found = types(
            &e,
            "иванов иван иванович родился 5 марта 1990 года, паспорт 4509123456",
        );
        assert!(
            found
                .iter()
                .any(|(t, s)| t == "FIO" && s == "иванов иван иванович"),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|(t, s)| t == "BIRTH_DATE" && s == "5 марта 1990"),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|(t, s)| t == "PASSPORT" && s == "4509123456"),
            "{found:?}"
        );
        let found = types(&e, "Дата рождения: 1987.05.12; выдан 2010-06-15");
        assert!(
            found
                .iter()
                .any(|(t, s)| t == "BIRTH_DATE" && s == "1987.05.12"),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .any(|(t, s)| t == "PASSPORT_ISSUE_DATE" && s == "2010-06-15"),
            "{found:?}"
        );
    }

    #[test]
    fn traps_are_not_personal_data() {
        let e = engine();
        assert!(types(
            &e,
            "Поэт Александр Пушкин родился 6 июня 1799 года в Москве"
        )
        .is_empty());
        assert!(types(
            &e,
            "Отделение банка по адресу г. Москва, ул. Каланчёвская, д. 27 работает до 20:00"
        )
        .is_empty());
        assert!(types(&e, "Пин-код 1234").iter().any(|(t, _)| t == "PIN"));
        assert!(types(&e, "Отчёт за 01.09.2026 готов").is_empty());
        assert!(types(&e, "В Москве открылся новый офис").is_empty());
        assert!(types(&e, "Горячая линия 8 800 200-00-00").is_empty());
    }

    #[test]
    fn checksum_failures_are_masked_when_context_vouches() {
        let e = engine();
        let sys = e.system("test").unwrap();
        let r = e.mask(
            "Карта 4276 1234 5678 9012, ИНН 123456789012, СНИЛС 123-456-789 00",
            sys,
        );
        assert_eq!(
            r.masked, "Карта 42** **** **** **12, ИНН 12********12, СНИЛС 12*-***-*** 00",
            "{}",
            r.masked
        );
        // Without context a failed checksum is still rejected.
        assert!(types(&e, "заказ 4276 1234 5678 9012 оформлен, код 123456789012").is_empty());
    }

    #[test]
    fn placeholders_are_consistent_per_identity() {
        let e = engine();
        let sys = e.system("llm").unwrap();
        let r = e.mask("Иванов Иван Иванович звонил с +7 925 123-45-67. Позже Иванова Ивана Ивановича перезвонили на 89251234567, а Петров Пётр Петрович — нет.", sys);
        assert_eq!(
            r.masked,
            "<FIO_1> звонил с <PHONE_1>. Позже <FIO_1> перезвонили на <PHONE_1>, а <FIO_2> — нет.",
            "{}",
            r.masked
        );
        let answer = "Клиент <FIO_1> просил перезвонить на <PHONE_1>; <FIO_2> не отвечал.";
        assert_eq!(
            e.demask(answer, &r.entries, false),
            "Клиент Иванов Иван Иванович просил перезвонить на +7 925 123-45-67; Петров Пётр Петрович не отвечал."
        );
    }
}
