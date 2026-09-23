//! The resolver turns detector candidates into accepted entities: it applies
//! context cues and suppressors, allowlists, the weak-entity policy,
//! combination rules, the per-system type selection and finally resolves
//! overlapping spans by priority.

use crate::config::{
    AddressMode, Allowlists, CombinationRule, SystemDef, TypeKind, WeakEntityPolicy,
};
use crate::detect::context::ContextMatcher;
use crate::detect::person::PUBLIC_PERSON;
use crate::detect::validators::digits;
use crate::dict::fold;
use crate::error::Result;
use crate::normalize::Normalized;
use crate::registry::TypeRegistry;
use crate::types::{pii, Candidate, Entity, PiiType, Span};
use std::collections::{HashMap, HashSet};

pub const PUBLIC_ADDRESS: &str = "PUBLIC_ADDRESS";
const PUBLIC_SUPPRESSION_WINDOW: usize = 80;

/// Per-system decisions the resolver needs, precomputed from [`SystemDef`].
#[derive(Clone, Debug)]
pub struct Policy {
    pub min_confidence: f32,
    pub weak_entities: WeakEntityPolicy,
    pub address_mode: AddressMode,
    pub combination_rules: Vec<CombinationRule>,
    enabled: HashSet<String>,
}

impl Policy {
    pub fn new(def: &SystemDef, registry: &TypeRegistry, default_min_confidence: f32) -> Self {
        let enabled = registry
            .defs()
            .iter()
            .filter(|d| {
                d.enabled && def.pii_types.includes(&d.id) && !def.exclude_types.contains(&d.id)
            })
            .map(|d| d.id.clone())
            .collect();
        Self {
            min_confidence: def.min_confidence.unwrap_or(default_min_confidence),
            weak_entities: def.weak_entities,
            address_mode: def.address_mode,
            combination_rules: def.combination_rules.clone(),
            enabled,
        }
    }

    pub fn allows(&self, ty: &str) -> bool {
        self.enabled.contains(ty)
    }
}

pub struct Resolver {
    registry: TypeRegistry,
    /// Context matchers of built-in (code) types; pattern types apply theirs
    /// inside the pattern detector.
    contexts: HashMap<String, ContextMatcher>,
    public_phones: HashSet<String>,
    public_phone_prefixes: Vec<String>,
    corporate_email_domains: Vec<String>,
    public_addresses: Vec<String>,
}

fn address_key(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in fold(s).chars() {
        if c.is_alphanumeric() {
            out.push(c);
        }
    }
    out
}

fn distance(a: &Span, b: &Span) -> usize {
    if a.overlaps(b) {
        0
    } else if a.end <= b.start {
        b.start - a.end
    } else {
        a.start - b.end
    }
}

impl Resolver {
    pub fn new(registry: TypeRegistry, allow: &Allowlists, default_window: usize) -> Result<Self> {
        let mut contexts = HashMap::new();
        for def in registry.defs() {
            if def.kind == TypeKind::Builtin {
                contexts.insert(
                    def.id.clone(),
                    ContextMatcher::compile(&def.context, default_window, &def.id)?,
                );
            }
        }
        Ok(Self {
            registry,
            contexts,
            public_phones: allow
                .public_phones
                .iter()
                .map(|p| {
                    digits(p)
                        .map(|d| char::from_digit(d, 10).unwrap())
                        .collect()
                })
                .collect(),
            public_phone_prefixes: allow.public_phone_prefixes.clone(),
            corporate_email_domains: allow
                .corporate_email_domains
                .iter()
                .map(|d| fold(d))
                .collect(),
            public_addresses: allow
                .public_addresses
                .iter()
                .map(|a| address_key(a))
                .collect(),
        })
    }

    pub fn registry(&self) -> &TypeRegistry {
        &self.registry
    }

    fn allowlisted(&self, text: &str, norm: &Normalized, c: &Candidate) -> bool {
        let value = &text[c.span.start..c.span.end];
        match c.ty.as_str() {
            pii::PHONE => {
                let d: String = digits(value)
                    .map(|x| char::from_digit(x, 10).unwrap())
                    .collect();
                let d = d
                    .strip_prefix('8')
                    .map(|rest| format!("7{rest}"))
                    .unwrap_or(d);
                self.public_phones.iter().any(|p| {
                    p.strip_prefix('8')
                        .map(|r| format!("7{r}"))
                        .unwrap_or_else(|| p.clone())
                        == d
                }) || self.public_phone_prefixes.iter().any(|p| {
                    let p = p
                        .strip_prefix('8')
                        .map(|r| format!("7{r}"))
                        .unwrap_or_else(|| p.clone());
                    d.starts_with(&p)
                })
            }
            pii::EMAIL => {
                let lower = fold(value);
                lower
                    .rsplit_once('@')
                    .map(|(_, domain)| {
                        self.corporate_email_domains
                            .iter()
                            .any(|d| domain == d || domain.ends_with(&format!(".{d}")))
                    })
                    .unwrap_or(false)
            }
            pii::ADDRESS => {
                let key = address_key(value);
                let _ = norm;
                self.public_addresses
                    .iter()
                    .any(|p| !p.is_empty() && (key.contains(p) || p.contains(&key)))
            }
            _ => false,
        }
    }

    /// Resolve candidates for one text under a system policy.
    pub fn resolve(
        &self,
        text: &str,
        norm: &Normalized,
        mut candidates: Vec<Candidate>,
        policy: &Policy,
    ) -> Vec<Entity> {
        let lower = norm.lower.as_str();
        let mut markers: Vec<(PiiType, Span)> = Vec::new();

        // 1. Context of built-in types: suppressors drop, cues boost.
        for c in candidates.iter_mut() {
            if c.detector == "pattern" || c.ty.is_address_component() {
                continue;
            }
            let Some(m) = self.contexts.get(c.ty.as_str()) else {
                continue;
            };
            let lstart = lower_offset(norm, c.span.start);
            let lend = lower_offset(norm, c.span.end);
            let r = m.check(lower, lstart, lend);
            if r.suppressed {
                c.confidence = 0.0;
                if c.ty == pii::FIO {
                    markers.push((PiiType::new(PUBLIC_PERSON), c.span));
                } else if c.ty == pii::ADDRESS {
                    markers.push((PiiType::new(PUBLIC_ADDRESS), c.span));
                }
                continue;
            }
            if r.matched {
                c.confidence = (c.confidence + m.boost).min(1.0);
                if c.weak {
                    c.weak = false;
                    c.evidence.push_str("+context");
                }
            }
        }

        // 2. Allowlists and public markers.
        for c in candidates.iter_mut() {
            if c.ty == PUBLIC_PERSON {
                markers.push((c.ty.clone(), c.span));
                c.confidence = 0.0;
            } else if c.confidence > 0.0 && self.allowlisted(text, norm, c) {
                c.confidence = 0.0;
                if c.ty == pii::ADDRESS {
                    markers.push((PiiType::new(PUBLIC_ADDRESS), c.span));
                }
            }
        }
        // Components of a public address go with it.
        let public_addresses: Vec<Span> = markers
            .iter()
            .filter(|(t, _)| *t == PUBLIC_ADDRESS)
            .map(|(_, s)| *s)
            .collect();
        for c in candidates.iter_mut() {
            if c.ty.is_address_component() && public_addresses.iter().any(|s| s.contains(&c.span)) {
                c.confidence = 0.0;
            }
        }

        // 3. Threshold, type selection and address mode.
        let mut accepted: Vec<Candidate> = candidates
            .into_iter()
            .filter(|c| c.confidence >= policy.min_confidence && policy.allows(c.ty.as_str()))
            .collect();
        match policy.address_mode {
            AddressMode::Whole => {
                let parents: Vec<Span> = accepted
                    .iter()
                    .filter(|c| c.ty == pii::ADDRESS)
                    .map(|c| c.span)
                    .collect();
                accepted.retain(|c| {
                    !(c.ty.is_address_component() && parents.iter().any(|p| p.contains(&c.span)))
                });
            }
            AddressMode::Components => {
                let parents: Vec<Span> = accepted
                    .iter()
                    .filter(|c| c.ty == pii::ADDRESS)
                    .map(|c| c.span)
                    .collect();
                let has_children = |p: &Span| {
                    accepted
                        .iter()
                        .any(|c| c.ty.is_address_component() && p.contains(&c.span))
                };
                let drop: Vec<Span> = parents
                    .iter()
                    .filter(|p| has_children(p))
                    .copied()
                    .collect();
                accepted.retain(|c| !(c.ty == pii::ADDRESS && drop.contains(&c.span)));
            }
        }

        // 4. Weak entities: policy, companions.
        let strong_types: HashSet<String> = accepted
            .iter()
            .filter(|c| !c.weak)
            .map(|c| c.ty.to_string())
            .collect();
        accepted.retain(|c| {
            if !c.weak {
                return true;
            }
            match policy.weak_entities {
                WeakEntityPolicy::Always => true,
                WeakEntityPolicy::Never => false,
                WeakEntityPolicy::ContextOnly => {
                    let companions = self.registry.companions(c.ty.as_str());
                    companions.iter().any(|x| strong_types.contains(x.as_str()))
                }
            }
        });

        // 5. Public figures: their birth dates/places are not client data.
        let public_persons: Vec<Span> = markers
            .iter()
            .filter(|(t, _)| *t == PUBLIC_PERSON)
            .map(|(_, s)| *s)
            .collect();
        if !public_persons.is_empty() {
            let fio_spans: Vec<Span> = accepted
                .iter()
                .filter(|c| c.ty == pii::FIO)
                .map(|c| c.span)
                .collect();
            accepted.retain(|c| {
                if !matches!(
                    c.ty.as_str(),
                    pii::BIRTH_DATE | pii::BIRTH_PLACE | pii::DATE | pii::CITIZENSHIP
                ) {
                    return true;
                }
                let near_public = public_persons
                    .iter()
                    .any(|p| distance(p, &c.span) <= PUBLIC_SUPPRESSION_WINDOW);
                let near_client = fio_spans
                    .iter()
                    .any(|p| distance(p, &c.span) <= PUBLIC_SUPPRESSION_WINDOW);
                !near_public || near_client
            });
        }

        // 6. Combination rules of the system.
        for rule in &policy.combination_rules {
            let present: Vec<Span> = accepted
                .iter()
                .filter(|c| rule.only_if_present.iter().any(|t| t == c.ty.as_str()))
                .map(|c| c.span)
                .collect();
            accepted.retain(|c| {
                if c.ty != rule.mask.as_str() {
                    return true;
                }
                if rule.window == 0 {
                    !present.is_empty()
                } else {
                    present.iter().any(|p| distance(p, &c.span) <= rule.window)
                }
            });
        }

        // 6b. A date right after an issuing authority is the issue date.
        let issuers: Vec<Span> = accepted
            .iter()
            .filter(|c| c.ty == pii::PASSPORT_ISSUER)
            .map(|c| c.span)
            .collect();
        for c in accepted.iter_mut() {
            if c.ty == pii::DATE
                && issuers.iter().any(|i| {
                    i.end <= c.span.start
                        && c.span.start - i.end <= 8
                        && text[i.end..c.span.start].chars().count() <= 4
                })
            {
                c.ty = PiiType::new(pii::PASSPORT_ISSUE_DATE);
                c.confidence = c.confidence.max(0.9);
                c.evidence = "date+after_issuer".into();
            }
        }

        // 7. Overlaps: evidence (confidence) first, then type priority, then length.
        accepted.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    self.registry
                        .priority(b.ty.as_str())
                        .cmp(&self.registry.priority(a.ty.as_str())),
                )
                .then(b.span.len().cmp(&a.span.len()))
                .then(a.span.start.cmp(&b.span.start))
        });
        // Accepted spans are disjoint, so an interval map answers "overlaps?" in O(log n).
        let mut taken: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
        let mut chosen: Vec<Candidate> = Vec::with_capacity(accepted.len());
        for c in accepted {
            let before = taken
                .range(..c.span.end)
                .next_back()
                .map(|(_, e)| *e > c.span.start)
                .unwrap_or(false);
            if before {
                continue;
            }
            taken.insert(c.span.start, c.span.end);
            chosen.push(c);
        }
        chosen.sort_by_key(|c| c.span.start);
        chosen
            .into_iter()
            .map(|c| Entity {
                ty: c.ty,
                start: c.span.start,
                end: c.span.end,
                confidence: c.confidence,
                evidence: c.evidence,
            })
            .collect()
    }
}

/// Folded offset for an original offset (inverse of the offset map, by scan).
fn lower_offset(norm: &Normalized, orig: usize) -> usize {
    norm.lower_offset(orig)
}
