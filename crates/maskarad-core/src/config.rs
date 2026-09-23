//! Configuration model of the engine: PII type definitions, dictionaries,
//! allowlists, masking profiles and consumer systems.
//!
//! Everything a consumer system can tune lives here; the engine is built from
//! a [`CoreConfig`] and never needs code changes to add a type or a system.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

fn default_true() -> bool {
    true
}
fn default_confidence() -> f32 {
    0.9
}
fn default_boost() -> f32 {
    0.15
}
fn default_context_window() -> usize {
    48
}
fn default_min_confidence() -> f32 {
    0.5
}
fn default_profile() -> String {
    "reference".to_string()
}
fn default_public_phone_prefixes() -> Vec<String> {
    vec!["8800".to_string(), "7800".to_string()]
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CoreConfig {
    #[serde(default)]
    pub detection: DetectionSettings,
    #[serde(default)]
    pub masking: MaskingSettings,
    /// Additional or overriding PII type definitions. Built-in definitions are
    /// merged in by id; a user definition with the same id replaces it.
    #[serde(default)]
    pub pii_types: Vec<PiiTypeDef>,
    #[serde(default)]
    pub dictionaries: DictConfig,
    #[serde(default)]
    pub allowlists: Allowlists,
    /// Masking profiles in addition to the built-in `reference`, `placeholder`,
    /// `synthetic` and `full`. A user profile with the same name replaces it.
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
    #[serde(default)]
    pub systems: Vec<SystemDef>,
}

impl CoreConfig {
    pub fn from_yaml_str(text: &str) -> crate::error::Result<Self> {
        Ok(serde_yaml::from_str(text)?)
    }

    pub fn system(&self, id: &str) -> Option<&SystemDef> {
        self.systems.iter().find(|s| s.id == id)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetectionSettings {
    /// Default size (in characters) of the context window before/after a match.
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    /// Candidates below this confidence are dropped regardless of the system.
    #[serde(default = "default_min_confidence")]
    pub min_confidence: f32,
}

impl Default for DetectionSettings {
    fn default() -> Self {
        Self {
            context_window: default_context_window(),
            min_confidence: default_min_confidence(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaskingSettings {
    /// Secret for deterministic `token` and `synthetic` strategies: the same
    /// value always maps to the same token across requests and replicas.
    /// Must be shared by all replicas; set it from a secret store.
    #[serde(default)]
    pub secret: Option<String>,
}

/// Definition of one PII type. Pattern types are fully data-driven; built-in
/// types are implemented in code but still carry their tunables here
/// (context words, confidence, priority, enabled flag).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiiTypeDef {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: TypeKind,
    /// Regular expressions applied to the case-folded text. A named group `v`
    /// selects the value; otherwise the whole match is the value.
    #[serde(default)]
    pub patterns: Vec<PatternSpec>,
    #[serde(default, with = "serde_yaml::with::singleton_map")]
    pub validator: Validator,
    #[serde(default)]
    pub context: ContextSpec,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// Higher priority wins when spans of different types overlap.
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Reject matches directly adjacent to other digits (part of a longer number).
    #[serde(default = "default_true")]
    pub digit_boundary: bool,
    /// The match alone is not personal data; it needs context or a companion
    /// entity (CVV, PIN, a bare date).
    #[serde(default)]
    pub weak: bool,
    /// A value that fails the checksum is still accepted (with lower
    /// confidence) when the type's context words are present: `карта 4276
    /// 1234 5678 9012` is a card number even if it fails Luhn.
    #[serde(default)]
    pub soft_validator: bool,
    /// Types whose presence anywhere in the text confirms a weak candidate
    /// of this type (a bare `770-001` next to a passport number).
    #[serde(default)]
    pub companions: Vec<String>,
    /// Free-form grouping label, e.g. `passport`, `card`, `address`.
    #[serde(default)]
    pub group: Option<String>,
}

/// One regular expression of a pattern type, optionally with its own tunables.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PatternSpec {
    Regex(String),
    Detailed {
        regex: String,
        /// This pattern alone is not enough: the type's context must be present.
        #[serde(default)]
        requires_context: bool,
        /// Confidence for matches of this pattern instead of the type default.
        #[serde(default)]
        confidence: Option<f32>,
    },
}

impl PatternSpec {
    pub fn regex(&self) -> &str {
        match self {
            PatternSpec::Regex(r) => r,
            PatternSpec::Detailed { regex, .. } => regex,
        }
    }

    pub fn requires_context(&self) -> bool {
        matches!(
            self,
            PatternSpec::Detailed {
                requires_context: true,
                ..
            }
        )
    }

    pub fn confidence(&self) -> Option<f32> {
        match self {
            PatternSpec::Regex(_) => None,
            PatternSpec::Detailed { confidence, .. } => *confidence,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeKind {
    #[default]
    Pattern,
    Builtin,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Validator {
    #[default]
    None,
    /// Luhn checksum over the digits of the value.
    Luhn,
    /// Russian INN checksum (10 or 12 digits).
    Inn,
    /// Russian SNILS checksum.
    Snils,
    /// Calendar validity of a date (any supported spelling).
    Date,
    /// Phone number plausibility (digit count and country prefix).
    Phone,
    /// Number of digits in the value within a range.
    DigitsLen { min: usize, max: usize },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSpec {
    /// Regex fragments (lower case) expected before the value.
    #[serde(default)]
    pub before: Vec<String>,
    /// Regex fragments expected after the value.
    #[serde(default)]
    pub after: Vec<String>,
    /// Window in characters; 0 means `detection.context_window`.
    #[serde(default)]
    pub window: usize,
    /// Without context the match is discarded.
    #[serde(default)]
    pub required: bool,
    /// Confidence added when context is present.
    #[serde(default = "default_boost")]
    pub boost: f32,
    /// Regex fragments that mark the match as *not* personal data
    /// (e.g. `горячая линия` before a phone number).
    #[serde(default)]
    pub suppress: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DictConfig {
    /// Directory with dictionary files replacing the embedded ones.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// Extra dictionary files merged into the embedded ones.
    #[serde(default)]
    pub extra: Vec<ExtraDict>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraDict {
    pub class: DictClass,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictClass {
    FirstName,
    Patronymic,
    Surname,
    City,
    Region,
    Country,
    PublicPerson,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowlists {
    /// `Фамилия|Имя|Отчество` (name and patronymic optional).
    #[serde(default)]
    pub public_persons: Vec<String>,
    /// Addresses of the organisation's own offices; never personal data.
    #[serde(default)]
    pub public_addresses: Vec<String>,
    /// Hotline and office numbers; never personal data.
    #[serde(default)]
    pub public_phones: Vec<String>,
    /// Digit prefixes of toll-free/public numbers (`8800…`), never personal data.
    #[serde(default = "default_public_phone_prefixes")]
    pub public_phone_prefixes: Vec<String>,
    /// E-mail domains of the organisation; addresses there are not client data.
    #[serde(default)]
    pub corporate_email_domains: Vec<String>,
    /// Extra cue words before a name that mark a public figure.
    #[serde(default)]
    pub public_person_cues: Vec<String>,
    /// Extra cue words around an address that mark a public place.
    #[serde(default)]
    pub public_address_cues: Vec<String>,
}

impl Default for Allowlists {
    fn default() -> Self {
        Self {
            public_persons: Vec::new(),
            public_addresses: Vec::new(),
            public_phones: Vec::new(),
            public_phone_prefixes: default_public_phone_prefixes(),
            corporate_email_domains: Vec::new(),
            public_person_cues: Vec::new(),
            public_address_cues: Vec::new(),
        }
    }
}

/// Which characters a positional mask replaces.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskChars {
    Digits,
    Letters,
    #[default]
    Alnum,
    All,
}

/// How a value of some type is replaced in the masked text.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StrategySpec {
    /// `Иванов Иван Иванович` → `И. И. И.`
    Initials { with_dots: bool },
    /// Keep a prefix and a suffix, fill the rest: `4509 123456` → `45** ****56`.
    Partial {
        keep_start: usize,
        keep_end: usize,
        fill: char,
        mask_chars: MaskChars,
    },
    /// Replace every masked character, keeping separators and length.
    Full { fill: char, mask_chars: MaskChars },
    /// Unique placeholder such as `<FIO_1>`; `{type}` and `{n}` are substituted.
    Placeholder { template: String },
    /// Deterministic realistic fake value of the same type.
    Synthetic,
    /// Opaque token derived from the value: `tok_9f2a1c…`.
    Token { prefix: String, length: usize },
    /// Delete the value.
    Remove,
}

impl StrategySpec {
    pub fn initials() -> Self {
        StrategySpec::Initials { with_dots: true }
    }

    pub fn partial(keep_start: usize, keep_end: usize) -> Self {
        StrategySpec::Partial {
            keep_start,
            keep_end,
            fill: '*',
            mask_chars: MaskChars::Alnum,
        }
    }

    pub fn partial_digits(keep_start: usize, keep_end: usize) -> Self {
        StrategySpec::Partial {
            keep_start,
            keep_end,
            fill: '*',
            mask_chars: MaskChars::Digits,
        }
    }

    pub fn full() -> Self {
        StrategySpec::Full {
            fill: '*',
            mask_chars: MaskChars::Alnum,
        }
    }

    pub fn placeholder() -> Self {
        StrategySpec::Placeholder {
            template: "<{type}_{n}>".to_string(),
        }
    }

    pub fn token() -> Self {
        StrategySpec::Token {
            prefix: "tok_".to_string(),
            length: 12,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "initials" => Self::initials(),
            "partial" => Self::partial(2, 2),
            "full" => Self::full(),
            "placeholder" => Self::placeholder(),
            "synthetic" => StrategySpec::Synthetic,
            "token" => Self::token(),
            "remove" => StrategySpec::Remove,
            _ => return None,
        })
    }
}

impl<'de> Deserialize<'de> for StrategySpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        fn star() -> char {
            '*'
        }
        fn two() -> usize {
            2
        }
        fn twelve() -> usize {
            12
        }
        fn tok() -> String {
            "tok_".to_string()
        }
        fn template() -> String {
            "<{type}_{n}>".to_string()
        }

        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Tagged {
            Initials {
                #[serde(default = "default_true")]
                with_dots: bool,
            },
            Partial {
                #[serde(default = "two")]
                keep_start: usize,
                #[serde(default = "two")]
                keep_end: usize,
                #[serde(default = "star")]
                fill: char,
                #[serde(default)]
                mask_chars: MaskChars,
            },
            Full {
                #[serde(default = "star")]
                fill: char,
                #[serde(default)]
                mask_chars: MaskChars,
            },
            Placeholder {
                #[serde(default = "template")]
                template: String,
            },
            Synthetic,
            Token {
                #[serde(default = "tok")]
                prefix: String,
                #[serde(default = "twelve")]
                length: usize,
            },
            Remove,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            Tagged(Tagged),
        }

        match Repr::deserialize(deserializer)? {
            Repr::Name(name) => StrategySpec::from_name(&name).ok_or_else(|| {
                D::Error::custom(format!(
                    "unknown strategy `{name}` (expected initials, partial, full, placeholder, synthetic, token or remove)"
                ))
            }),
            Repr::Tagged(t) => Ok(match t {
                Tagged::Initials { with_dots } => StrategySpec::Initials { with_dots },
                Tagged::Partial { keep_start, keep_end, fill, mask_chars } => {
                    StrategySpec::Partial { keep_start, keep_end, fill, mask_chars }
                }
                Tagged::Full { fill, mask_chars } => StrategySpec::Full { fill, mask_chars },
                Tagged::Placeholder { template } => StrategySpec::Placeholder { template },
                Tagged::Synthetic => StrategySpec::Synthetic,
                Tagged::Token { prefix, length } => StrategySpec::Token { prefix, length },
                Tagged::Remove => StrategySpec::Remove,
            }),
        }
    }
}

/// A masking profile: strategy per type with a fallback.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    pub description: String,
    #[serde(default = "StrategySpec::full")]
    pub default: StrategySpec,
    #[serde(default)]
    pub types: HashMap<String, StrategySpec>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            description: String::new(),
            default: StrategySpec::full(),
            types: HashMap::new(),
        }
    }
}

impl Profile {
    pub fn strategy_for(&self, ty: &str) -> &StrategySpec {
        self.types.get(ty).unwrap_or(&self.default)
    }
}

/// A consumer system and everything that is configurable per system.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemDef {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Disabled systems are rejected at the door.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub auth: AuthSpec,
    /// Types to detect and mask for this system: `all` or an explicit list.
    #[serde(default)]
    pub pii_types: TypeSelection,
    #[serde(default)]
    pub exclude_types: Vec<String>,
    /// Whether the system may demask.
    #[serde(default = "default_true")]
    pub demask: bool,
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Per-type strategy overrides on top of the profile.
    #[serde(default)]
    pub overrides: HashMap<String, StrategySpec>,
    #[serde(default)]
    pub combination_rules: Vec<CombinationRule>,
    #[serde(default)]
    pub weak_entities: WeakEntityPolicy,
    #[serde(default)]
    pub address_mode: AddressMode,
    /// Overrides `detection.min_confidence` for this system.
    #[serde(default)]
    pub min_confidence: Option<f32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSpec {
    /// Requests without a key are attributed to this system (used for the
    /// `/process` contract). At most one system may be anonymous.
    #[serde(default)]
    pub anonymous: bool,
    /// `sha256:<hex>` digests or plain keys (plain keys are discouraged).
    #[serde(default)]
    pub api_keys: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TypeSelection {
    Keyword(String),
    List(Vec<String>),
}

impl Default for TypeSelection {
    fn default() -> Self {
        TypeSelection::Keyword("all".to_string())
    }
}

impl TypeSelection {
    pub fn includes(&self, ty: &str) -> bool {
        match self {
            TypeSelection::Keyword(k) => k.eq_ignore_ascii_case("all"),
            TypeSelection::List(list) => list.iter().any(|t| t == ty),
        }
    }
}

/// Mask a type only when at least one of the companion types is present.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombinationRule {
    pub mask: String,
    pub only_if_present: Vec<String>,
    /// Distance in characters within which the companion must occur; 0 = whole text.
    #[serde(default)]
    pub window: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WeakEntityPolicy {
    /// Weak candidates are kept only with context or a companion entity.
    #[default]
    ContextOnly,
    /// Weak candidates are always masked (maximum recall).
    Always,
    /// Weak candidates are never masked.
    Never,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressMode {
    /// The whole address is one entity.
    #[default]
    Whole,
    /// Each component (city, street, house, …) is masked separately.
    Components,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_accepts_shorthand_and_tagged_forms() {
        let yaml = r#"
default: initials
types:
  PASSPORT: { kind: partial, keep_start: 2, keep_end: 2, mask_chars: digits }
  EMAIL: full
  FIO: { kind: placeholder }
"#;
        let p: Profile = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(p.default, StrategySpec::initials());
        assert_eq!(
            p.strategy_for("PASSPORT"),
            &StrategySpec::partial_digits(2, 2)
        );
        assert_eq!(p.strategy_for("EMAIL"), &StrategySpec::full());
        assert_eq!(p.strategy_for("FIO"), &StrategySpec::placeholder());
        assert_eq!(p.strategy_for("PHONE"), &StrategySpec::initials());
    }

    #[test]
    fn type_selection_parses_keyword_and_list() {
        let a: TypeSelection = serde_yaml::from_str("all").unwrap();
        assert!(a.includes("FIO"));
        let l: TypeSelection = serde_yaml::from_str("[FIO, PHONE]").unwrap();
        assert!(l.includes("FIO"));
        assert!(!l.includes("EMAIL"));
    }

    #[test]
    fn unknown_strategy_is_rejected() {
        let err = serde_yaml::from_str::<StrategySpec>("bogus").unwrap_err();
        assert!(err.to_string().contains("unknown strategy"));
    }
}
