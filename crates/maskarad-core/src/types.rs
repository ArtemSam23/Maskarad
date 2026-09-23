//! Core value types shared by detectors, resolver, masker and the server.

use crate::sensitive::Sensitive;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Identifiers of the built-in PII types. Custom types defined in configuration
/// use arbitrary identifiers; these constants exist so that code paths with
/// special handling (dates, addresses, names) refer to a single spelling.
pub mod pii {
    pub const FIO: &str = "FIO";
    pub const BIRTH_DATE: &str = "BIRTH_DATE";
    pub const BIRTH_PLACE: &str = "BIRTH_PLACE";
    pub const PASSPORT: &str = "PASSPORT";
    pub const CITIZENSHIP: &str = "CITIZENSHIP";
    pub const PASSPORT_ISSUER: &str = "PASSPORT_ISSUER";
    pub const SUBDIVISION_CODE: &str = "SUBDIVISION_CODE";
    pub const PASSPORT_ISSUE_DATE: &str = "PASSPORT_ISSUE_DATE";
    pub const DRIVER_LICENSE: &str = "DRIVER_LICENSE";
    pub const ADDRESS: &str = "ADDRESS";
    pub const ADDRESS_COUNTRY: &str = "ADDRESS_COUNTRY";
    pub const ADDRESS_INDEX: &str = "ADDRESS_INDEX";
    pub const ADDRESS_REGION: &str = "ADDRESS_REGION";
    pub const ADDRESS_CITY: &str = "ADDRESS_CITY";
    pub const ADDRESS_STREET: &str = "ADDRESS_STREET";
    pub const ADDRESS_HOUSE: &str = "ADDRESS_HOUSE";
    pub const ADDRESS_APARTMENT: &str = "ADDRESS_APARTMENT";
    pub const EMAIL: &str = "EMAIL";
    pub const PHONE: &str = "PHONE";
    pub const INN: &str = "INN";
    pub const CARD_NUMBER: &str = "CARD_NUMBER";
    pub const CVV: &str = "CVV";
    pub const PIN: &str = "PIN";
    pub const CARD_HOLDER: &str = "CARD_HOLDER";
    pub const DATE: &str = "DATE";
    pub const SNILS: &str = "SNILS";
    pub const FOREIGN_PASSPORT: &str = "FOREIGN_PASSPORT";
    pub const MILITARY_ID: &str = "MILITARY_ID";
    pub const BIRTH_CERTIFICATE: &str = "BIRTH_CERTIFICATE";
    pub const RESIDENCE_PERMIT: &str = "RESIDENCE_PERMIT";
    pub const OMS_POLICY: &str = "OMS_POLICY";
    pub const BANK_ACCOUNT: &str = "BANK_ACCOUNT";
    pub const CARD_EXPIRY: &str = "CARD_EXPIRY";

    /// Types that are components of a larger address entity.
    pub const ADDRESS_COMPONENTS: &[&str] = &[
        ADDRESS_COUNTRY,
        ADDRESS_INDEX,
        ADDRESS_REGION,
        ADDRESS_CITY,
        ADDRESS_STREET,
        ADDRESS_HOUSE,
        ADDRESS_APARTMENT,
    ];
}

/// Identifier of a PII type, e.g. `FIO` or `CARD_NUMBER`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PiiType(String);

impl PiiType {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_address_component(&self) -> bool {
        pii::ADDRESS_COMPONENTS.contains(&self.0.as_str())
    }
}

impl From<&str> for PiiType {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for PiiType {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl PartialEq<str> for PiiType {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for PiiType {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl fmt::Display for PiiType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PiiType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Half-open byte range `[start, end)` in the original text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start <= end, "span start must not exceed end");
        Self { start, end }
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    pub fn overlaps(&self, other: &Span) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn contains(&self, other: &Span) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// Raw output of a detector before the resolver decides what is really
/// personal data. Spans are byte offsets in the original text.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub span: Span,
    pub ty: PiiType,
    /// 0..=1, how sure the detector is that the span is this type of data.
    pub confidence: f32,
    /// Name of the detector that produced the candidate.
    pub detector: &'static str,
    /// Short PII-free description of the evidence, e.g. `patronymic+surname`.
    pub evidence: String,
    /// The candidate is only personal data when supported by context or by
    /// other entities in the same text (CVV alone, a bare date, a lone city).
    pub weak: bool,
}

impl Candidate {
    pub fn new(
        span: Span,
        ty: impl Into<PiiType>,
        confidence: f32,
        detector: &'static str,
    ) -> Self {
        Self {
            span,
            ty: ty.into(),
            confidence,
            detector,
            evidence: String::new(),
            weak: false,
        }
    }

    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence = evidence.into();
        self
    }

    pub fn weak(mut self) -> Self {
        self.weak = true;
        self
    }
}

/// A resolved entity: the resolver accepted the candidate as personal data.
/// Serializable, but deliberately does not carry the text of the entity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    #[serde(rename = "type")]
    pub ty: PiiType,
    pub start: usize,
    pub end: usize,
    pub confidence: f32,
    pub evidence: String,
}

impl Entity {
    pub fn span(&self) -> Span {
        Span::new(self.start, self.end)
    }
}

/// One replacement performed by the masker. Stored (encrypted) to allow
/// demasking. `original` is the only place where the protected value lives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaskEntry {
    #[serde(rename = "type")]
    pub ty: PiiType,
    /// Byte offsets of the entity in the original text.
    pub start: usize,
    pub end: usize,
    /// Byte offsets of the replacement in the masked text.
    pub masked_start: usize,
    pub masked_end: usize,
    pub mask: String,
    pub original: Sensitive<String>,
}

/// Result of masking one text.
#[derive(Clone, Debug)]
pub struct MaskResult {
    pub masked: String,
    pub entries: Vec<MaskEntry>,
    pub entities: Vec<Entity>,
}

impl MaskResult {
    /// Names of detected types with counts, in a stable order, for logging.
    pub fn type_counts(&self) -> Vec<(PiiType, usize)> {
        let mut counts: Vec<(PiiType, usize)> = Vec::new();
        for e in &self.entities {
            match counts.iter_mut().find(|(t, _)| *t == e.ty) {
                Some((_, n)) => *n += 1,
                None => counts.push((e.ty.clone(), 1)),
            }
        }
        counts.sort();
        counts
    }
}
