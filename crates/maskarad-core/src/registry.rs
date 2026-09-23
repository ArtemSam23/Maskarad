//! Registry of PII type definitions: the built-in catalogue merged with the
//! user configuration.

use crate::config::PiiTypeDef;
use crate::error::Result;
use serde::Deserialize;
use std::collections::HashMap;

const BUILTIN_TYPES: &str = include_str!("../builtin/types.yaml");

#[derive(Deserialize)]
struct BuiltinFile {
    pii_types: Vec<PiiTypeDef>,
}

#[derive(Clone, Debug, Default)]
pub struct TypeRegistry {
    defs: Vec<PiiTypeDef>,
    index: HashMap<String, usize>,
}

impl TypeRegistry {
    pub fn builtin() -> Result<Self> {
        let file: BuiltinFile = serde_yaml::from_str(BUILTIN_TYPES)?;
        let mut reg = Self::default();
        for def in file.pii_types {
            reg.insert(def);
        }
        Ok(reg)
    }

    /// Built-in catalogue with user definitions merged in (same id replaces).
    pub fn with_overrides(user: &[PiiTypeDef]) -> Result<Self> {
        let mut reg = Self::builtin()?;
        for def in user {
            reg.insert(def.clone());
        }
        Ok(reg)
    }

    fn insert(&mut self, def: PiiTypeDef) {
        match self.index.get(&def.id) {
            Some(&i) => self.defs[i] = def,
            None => {
                self.index.insert(def.id.clone(), self.defs.len());
                self.defs.push(def);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&PiiTypeDef> {
        self.index.get(id).map(|&i| &self.defs[i])
    }

    pub fn defs(&self) -> &[PiiTypeDef] {
        &self.defs
    }

    pub fn priority(&self, id: &str) -> i32 {
        self.get(id).map(|d| d.priority).unwrap_or(0)
    }

    pub fn is_weak(&self, id: &str) -> bool {
        self.get(id).map(|d| d.weak).unwrap_or(false)
    }

    pub fn companions(&self, id: &str) -> &[String] {
        self.get(id).map(|d| d.companions.as_slice()).unwrap_or(&[])
    }

    pub fn is_enabled(&self, id: &str) -> bool {
        self.get(id).map(|d| d.enabled).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TypeKind;

    #[test]
    fn builtin_catalogue_parses_and_compiles() {
        let reg = TypeRegistry::builtin().unwrap();
        for id in [
            "FIO",
            "PASSPORT",
            "CARD_NUMBER",
            "EMAIL",
            "PHONE",
            "ADDRESS",
            "CVV",
            "PIN",
        ] {
            assert!(reg.get(id).is_some(), "{id} missing");
        }
        for def in reg.defs() {
            if def.kind == TypeKind::Pattern {
                for p in &def.patterns {
                    regex::Regex::new(p.regex()).unwrap_or_else(|e| panic!("{}: {e}", def.id));
                }
            }
        }
    }

    #[test]
    fn user_definition_overrides_builtin() {
        let user = vec![PiiTypeDef {
            id: "EMAIL".into(),
            description: String::new(),
            kind: TypeKind::Pattern,
            patterns: vec![],
            validator: Default::default(),
            context: Default::default(),
            confidence: 0.5,
            priority: 1,
            enabled: false,
            digit_boundary: true,
            weak: false,
            soft_validator: false,
            companions: vec![],
            group: None,
        }];
        let reg = TypeRegistry::with_overrides(&user).unwrap();
        assert!(!reg.is_enabled("EMAIL"));
        assert_eq!(reg.priority("EMAIL"), 1);
    }
}
