//! Wrapper for values that must never reach logs, metrics or error messages.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A value that is personal data. It has no `Display` implementation and its
/// `Debug` output is always `[REDACTED]`, so it cannot leak through logging
/// macros or error formatting by accident. The inner value is only reachable
/// through [`Sensitive::expose`].
#[derive(Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Deliberate access to the protected value.
    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T> From<T> for Sensitive<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_value() {
        let s = Sensitive::new(String::from("4509 123456"));
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(s.expose(), "4509 123456");
    }
}
