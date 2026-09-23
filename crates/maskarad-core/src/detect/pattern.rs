//! Data-driven detector: regular expressions with validators and context
//! windows, entirely defined by [`PiiTypeDef`] entries.
//!
//! Each pattern scans the folded text with the regex crate's DFA; capture
//! groups are extracted only on matched slices. Patterns must stay
//! DFA-friendly: use `(?-u:\b)` next to digits or Latin letters and a
//! consuming prefix such as `(?:^|[^а-яa-z0-9])` before Cyrillic words.

use super::context::ContextMatcher;
use super::validators;
use super::{digit_boundary, DetectCtx, Detector};
use crate::config::{PiiTypeDef, TypeKind, Validator};
use crate::error::{CoreError, Result};
use crate::types::{Candidate, PiiType};
use regex::Regex;

struct CompiledType {
    id: PiiType,
    confidence: f32,
    validator: Validator,
    ctx: ContextMatcher,
    digit_boundary: bool,
    weak: bool,
    soft_validator: bool,
}

struct CompiledPattern {
    ty: usize,
    re: Regex,
    /// The pattern names a `v` group selecting the value.
    has_value_group: bool,
    requires_context: bool,
    confidence: Option<f32>,
}

pub struct PatternDetector {
    types: Vec<CompiledType>,
    patterns: Vec<CompiledPattern>,
}

impl PatternDetector {
    /// Compiles every enabled pattern-kind definition.
    pub fn compile(defs: &[PiiTypeDef], default_window: usize) -> Result<Self> {
        let mut types = Vec::new();
        let mut patterns = Vec::new();
        for def in defs
            .iter()
            .filter(|d| d.enabled && d.kind == TypeKind::Pattern)
        {
            let ty_index = types.len();
            types.push(CompiledType {
                id: PiiType::new(def.id.clone()),
                confidence: def.confidence,
                validator: def.validator.clone(),
                ctx: ContextMatcher::compile(&def.context, default_window, &def.id)?,
                digit_boundary: def.digit_boundary,
                weak: def.weak,
                soft_validator: def.soft_validator,
            });
            for spec in &def.patterns {
                let re = Regex::new(spec.regex()).map_err(|source| CoreError::Regex {
                    ty: def.id.clone(),
                    source,
                })?;
                let has_value_group = re.capture_names().any(|n| n == Some("v"));
                patterns.push(CompiledPattern {
                    ty: ty_index,
                    re,
                    has_value_group,
                    requires_context: spec.requires_context(),
                    confidence: spec.confidence(),
                });
            }
        }
        Ok(Self { types, patterns })
    }

    pub fn type_count(&self) -> usize {
        self.types.len()
    }
}

impl Detector for PatternDetector {
    fn name(&self) -> &'static str {
        "pattern"
    }

    fn detect(&self, ctx: &DetectCtx<'_>, out: &mut Vec<Candidate>) {
        let lower = ctx.norm.lower.as_str();
        // Every pattern scans on its own: a badly written pattern then slows
        // down only itself, not a shared prefilter automaton.
        let profile = std::env::var("MASKARAD_PROFILE_PATTERNS").is_ok();
        for pat in &self.patterns {
            let ty = &self.types[pat.ty];
            let pat_started = std::time::Instant::now();
            let before = out.len();
            // `find_iter` runs on the fast DFA; capture extraction (slow path) is
            // done only on the matched slice.
            for m in pat.re.find_iter(lower) {
                let (start, end) = if pat.has_value_group {
                    match pat
                        .re
                        .captures(&lower[m.start()..m.end()])
                        .and_then(|c| c.name("v"))
                    {
                        Some(v) => (m.start() + v.start(), m.start() + v.end()),
                        None => (m.start(), m.end()),
                    }
                } else {
                    (m.start(), m.end())
                };
                if start == end {
                    continue;
                }
                if ty.digit_boundary && !digit_boundary(lower, start, end) {
                    continue;
                }
                let value = &lower[start..end];
                let valid = validators::validate(&ty.validator, value);
                let cr = ty.ctx.check(lower, start, end);
                if cr.suppressed {
                    continue;
                }
                // A failed checksum is fatal unless the type allows a soft
                // validator and the context vouches for the value.
                let vouched = ty.soft_validator && cr.matched;
                if !valid && !vouched {
                    continue;
                }
                if (ty.ctx.required || pat.requires_context) && !cr.matched {
                    continue;
                }
                let base = pat.confidence.unwrap_or(ty.confidence);
                let confidence = match (cr.matched, valid) {
                    (true, true) => (base + ty.ctx.boost).min(1.0),
                    (true, false) => (base - 0.2).max(0.5),
                    (false, _) => base,
                };
                let evidence = match (cr.matched, valid, &ty.validator) {
                    (true, _, Validator::None) => "pattern+context",
                    (true, true, _) => "pattern+validator+context",
                    (true, false, _) => "pattern+context(checksum failed)",
                    (false, _, Validator::None) => "pattern",
                    (false, _, _) => "pattern+validator",
                };
                let mut cand = Candidate::new(
                    ctx.norm.to_orig_span(start, end),
                    ty.id.clone(),
                    confidence,
                    "pattern",
                )
                .with_evidence(evidence);
                if ty.weak && !cr.matched {
                    cand = cand.weak();
                }
                out.push(cand);
            }
            if profile {
                eprintln!(
                    "    {:<18} {:?} {} candidates  {}",
                    ty.id,
                    pat_started.elapsed(),
                    out.len() - before,
                    pat.re.as_str()
                );
            }
        }
    }
}
