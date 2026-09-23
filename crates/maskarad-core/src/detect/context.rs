//! Context windows around a match: cue words before/after and suppressors.

use crate::config::ContextSpec;
use crate::error::{CoreError, Result};
use regex::Regex;

#[derive(Clone, Debug)]
pub struct ContextMatcher {
    before: Option<Regex>,
    after: Option<Regex>,
    suppress: Option<Regex>,
    pub window: usize,
    pub required: bool,
    pub boost: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ContextResult {
    pub matched: bool,
    pub suppressed: bool,
}

/// Slice of up to `chars` characters before byte offset `at`.
pub fn window_before(lower: &str, at: usize, chars: usize) -> &str {
    let mut start = at;
    for (n, (i, _)) in lower[..at].char_indices().rev().enumerate() {
        start = i;
        if n + 1 >= chars {
            break;
        }
    }
    &lower[start..at]
}

/// Slice of up to `chars` characters after byte offset `at`.
pub fn window_after(lower: &str, at: usize, chars: usize) -> &str {
    let mut end = at;
    for (n, (i, c)) in lower[at..].char_indices().enumerate() {
        end = at + i + c.len_utf8();
        if n + 1 >= chars {
            break;
        }
    }
    &lower[at..end]
}

pub fn compile_alternation(frags: &[String], ty: &str) -> Result<Option<Regex>> {
    if frags.is_empty() {
        return Ok(None);
    }
    let pattern = format!("(?:{})", frags.join("|"));
    Regex::new(&pattern)
        .map(Some)
        .map_err(|source| CoreError::Regex {
            ty: ty.to_string(),
            source,
        })
}

impl ContextMatcher {
    pub fn compile(spec: &ContextSpec, default_window: usize, ty: &str) -> Result<Self> {
        Ok(Self {
            before: compile_alternation(&spec.before, ty)?,
            after: compile_alternation(&spec.after, ty)?,
            suppress: compile_alternation(&spec.suppress, ty)?,
            window: if spec.window == 0 {
                default_window
            } else {
                spec.window
            },
            required: spec.required,
            boost: spec.boost,
        })
    }

    pub fn has_cues(&self) -> bool {
        self.before.is_some() || self.after.is_some()
    }

    pub fn check(&self, lower: &str, start: usize, end: usize) -> ContextResult {
        let before = window_before(lower, start, self.window);
        let after = window_after(lower, end, self.window);
        let suppressed = self
            .suppress
            .as_ref()
            .map(|r| r.is_match(before) || r.is_match(after))
            .unwrap_or(false);
        let matched = self
            .before
            .as_ref()
            .map(|r| r.is_match(before))
            .unwrap_or(false)
            || self
                .after
                .as_ref()
                .map(|r| r.is_match(after))
                .unwrap_or(false);
        ContextResult {
            matched,
            suppressed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_respect_char_boundaries() {
        let s = "паспорт 4509";
        let at = s.find("4509").unwrap();
        assert_eq!(window_before(s, at, 3), "рт ");
        assert_eq!(window_after(s, 0, 4), "пасп");
        assert_eq!(window_before(s, 0, 5), "");
    }

    #[test]
    fn context_matching() {
        let spec = ContextSpec {
            before: vec!["паспорт".into()],
            suppress: vec!["горячая линия".into()],
            ..Default::default()
        };
        let m = ContextMatcher::compile(&spec, 20, "T").unwrap();
        let s = "паспорт 4509 123456";
        let r = m.check(s, s.find("4509").unwrap(), s.len());
        assert!(r.matched && !r.suppressed);
        let s2 = "горячая линия 4509 123456";
        let r2 = m.check(s2, s2.find("4509").unwrap(), s2.len());
        assert!(!r2.matched && r2.suppressed);
    }
}
