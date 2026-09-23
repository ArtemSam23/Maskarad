//! Restoring original values into masked text.

use crate::types::MaskEntry;
use std::collections::HashMap;

/// The masked text is exactly what the masker produced: restore by position.
/// Returns `None` if the text does not match the recorded replacements.
pub fn demask_exact(masked: &str, entries: &[MaskEntry]) -> Option<String> {
    let mut out = String::with_capacity(masked.len() + entries.len() * 8);
    let mut cursor = 0;
    let mut sorted: Vec<&MaskEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| e.masked_start);
    for e in sorted {
        if e.masked_start < cursor
            || e.masked_end > masked.len()
            || !masked.is_char_boundary(e.masked_start)
        {
            return None;
        }
        if masked[e.masked_start..e.masked_end] != e.mask {
            return None;
        }
        out.push_str(&masked[cursor..e.masked_start]);
        out.push_str(e.original.expose());
        cursor = e.masked_end;
    }
    out.push_str(&masked[cursor..]);
    Some(out)
}

/// The text was rewritten (an LLM answer): substitute every occurrence of
/// every mask. Longer masks first; identical masks are restored in order of
/// appearance (the k-th occurrence gets the k-th original).
pub fn demask_substitute(text: &str, entries: &[MaskEntry]) -> String {
    let mut by_mask: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for e in entries {
        if e.mask.is_empty() || e.mask == *e.original.expose() {
            continue;
        }
        let list = by_mask.entry(e.mask.as_str()).or_default();
        if list.is_empty() {
            order.push(e.mask.as_str());
        }
        list.push(e.original.expose().as_str());
    }
    order.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    let mut current = text.to_string();
    for mask in order {
        let originals = &by_mask[mask];
        let mut k = 0;
        current = replace_occurrences(&current, mask, originals, &mut k, false);
        if mask.chars().any(|c| c.is_alphabetic()) {
            // Placeholders sometimes come back in a different case.
            current = replace_occurrences(&current, mask, originals, &mut k, true);
        }
    }
    current
}

/// Replaces every occurrence of `mask` in `text`, the k-th with the k-th original.
fn replace_occurrences(
    text: &str,
    mask: &str,
    originals: &[&str],
    k: &mut usize,
    case_insensitive: bool,
) -> String {
    let haystack: std::borrow::Cow<'_, str> = if case_insensitive {
        let lower = text.to_lowercase();
        if lower.len() != text.len() {
            return text.to_string();
        }
        lower.into()
    } else {
        text.into()
    };
    let needle: std::borrow::Cow<'_, str> = if case_insensitive {
        mask.to_lowercase().into()
    } else {
        mask.into()
    };
    if needle.len() != mask.len() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(rel) = haystack[cursor..].find(needle.as_ref()) {
        let at = cursor + rel;
        out.push_str(&text[cursor..at]);
        out.push_str(originals[(*k).min(originals.len() - 1)]);
        *k += 1;
        cursor = at + mask.len();
    }
    out.push_str(&text[cursor..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sensitive::Sensitive;
    use crate::types::PiiType;

    fn entry(mask: &str, original: &str, ms: usize) -> MaskEntry {
        MaskEntry {
            ty: PiiType::new("FIO"),
            start: 0,
            end: 0,
            masked_start: ms,
            masked_end: ms + mask.len(),
            mask: mask.to_string(),
            original: Sensitive::new(original.to_string()),
        }
    }

    #[test]
    fn substitute_handles_prefix_masks_and_order() {
        let entries = vec![
            entry("<FIO_1>", "Иванов", 0),
            entry("<FIO_10>", "Петров", 0),
            entry("45** ****56", "4509 123456", 0),
        ];
        let text = "Ответ: <FIO_10> и <FIO_1>, паспорт 45** ****56; ещё раз <fio_1>.";
        let out = demask_substitute(text, &entries);
        assert_eq!(
            out,
            "Ответ: Петров и Иванов, паспорт 4509 123456; ещё раз Иванов."
        );
    }

    #[test]
    fn exact_restores_by_position() {
        let masked = "Клиент И. И. И., паспорт 45** ****56";
        let e1 = entry(
            "И. И. И.",
            "Иванов Иван Иванович",
            masked.find("И. И. И.").unwrap(),
        );
        let e2 = entry("45** ****56", "4509 123456", masked.find("45**").unwrap());
        assert_eq!(
            demask_exact(masked, &[e1, e2]).unwrap(),
            "Клиент Иванов Иван Иванович, паспорт 4509 123456"
        );
        assert!(demask_exact("changed text", &[entry("И.", "И", 0)]).is_none());
    }
}
