//! Quality evaluation against a golden dataset: per-type precision/recall,
//! leak rate (expected PII characters left unmasked), normalised Levenshtein
//! similarity of the masked text (the checker's metric) and demask exactness.

use crate::golden::GoldenRecord;
use maskarad_core::{CompiledSystem, Engine, Entity};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Default, Clone, Serialize)]
pub struct TypeStats {
    pub tp: usize,
    pub fp: usize,
    pub fn_: usize,
}

impl TypeStats {
    pub fn precision(&self) -> f64 {
        if self.tp + self.fp == 0 {
            1.0
        } else {
            self.tp as f64 / (self.tp + self.fp) as f64
        }
    }
    pub fn recall(&self) -> f64 {
        if self.tp + self.fn_ == 0 {
            1.0
        } else {
            self.tp as f64 / (self.tp + self.fn_) as f64
        }
    }
    pub fn f1(&self) -> f64 {
        let (p, r) = (self.precision(), self.recall());
        if p + r == 0.0 {
            0.0
        } else {
            2.0 * p * r / (p + r)
        }
    }
}

#[derive(Serialize)]
pub struct Report {
    pub records: usize,
    pub by_type: BTreeMap<String, TypeStats>,
    pub micro: TypeStats,
    pub leak_chars: usize,
    pub expected_chars: usize,
    pub leaked_records: usize,
    pub false_positive_records: usize,
    pub negative_records: usize,
    pub negative_clean: usize,
    pub mask_similarity: f64,
    pub demask_exact: usize,
    pub failures: Vec<Failure>,
}

#[derive(Serialize, Clone)]
pub struct Failure {
    pub id: String,
    pub category: String,
    pub kind: String,
    pub detail: String,
}

fn iou(a: (usize, usize), b: (usize, usize)) -> f64 {
    let inter = a.1.min(b.1).saturating_sub(a.0.max(b.0));
    let union = a.1.max(b.1) - a.0.min(b.0);
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

pub fn similarity(a: &str, b: &str) -> f64 {
    let max = a.chars().count().max(b.chars().count());
    if max == 0 {
        1.0
    } else {
        1.0 - levenshtein(a, b) as f64 / max as f64
    }
}

/// Address components count as their parent when the system masks whole addresses.
fn canonical_type(ty: &str) -> &str {
    if maskarad_core::types::pii::ADDRESS_COMPONENTS.contains(&ty) {
        "ADDRESS"
    } else {
        ty
    }
}

pub fn evaluate(
    engine: &Engine,
    system: &CompiledSystem,
    records: &[GoldenRecord],
    max_failures: usize,
) -> Report {
    let mut by_type: BTreeMap<String, TypeStats> = BTreeMap::new();
    let mut micro = TypeStats::default();
    let mut leak_chars = 0;
    let mut expected_chars = 0;
    let mut leaked_records = 0;
    let mut false_positive_records = 0;
    let mut negative_records = 0;
    let mut negative_clean = 0;
    let mut similarity_sum = 0.0;
    let mut demask_exact = 0;
    let mut failures = Vec::new();

    for rec in records {
        let result = engine.mask(&rec.text, system);
        let predicted: Vec<Entity> = result.entities.clone();
        let mut matched_pred = vec![false; predicted.len()];
        let mut record_leaked = false;
        for exp in &rec.entities {
            expected_chars += exp.end - exp.start;
            let ety = canonical_type(&exp.ty);
            let best = predicted
                .iter()
                .enumerate()
                .filter(|(i, p)| !matched_pred[*i] && canonical_type(p.ty.as_str()) == ety)
                .map(|(i, p)| (i, iou((exp.start, exp.end), (p.start, p.end))))
                .filter(|(_, s)| *s >= 0.5)
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
            let stats = by_type.entry(ety.to_string()).or_default();
            match best {
                Some((i, _)) => {
                    matched_pred[i] = true;
                    stats.tp += 1;
                    micro.tp += 1;
                }
                None => {
                    stats.fn_ += 1;
                    micro.fn_ += 1;
                    if failures.len() < max_failures {
                        failures.push(Failure {
                            id: rec.id.clone(),
                            category: rec.category.clone(),
                            kind: "missed".into(),
                            detail: format!("{ety} expected at {}..{}", exp.start, exp.end),
                        });
                    }
                }
            }
            // Leak: expected characters not covered by any predicted entity.
            let mut uncovered = 0;
            for pos in exp.start..exp.end {
                if !predicted.iter().any(|p| p.start <= pos && pos < p.end)
                    && rec.text.is_char_boundary(pos)
                {
                    uncovered += 1;
                }
            }
            if uncovered > 0 {
                leak_chars += uncovered;
                record_leaked = true;
                if best.is_some() && failures.len() < max_failures {
                    let covered: Vec<String> = predicted
                        .iter()
                        .filter(|p| p.start < exp.end && exp.start < p.end)
                        .map(|p| format!("{} {}..{}", p.ty, p.start, p.end))
                        .collect();
                    failures.push(Failure {
                        id: rec.id.clone(),
                        category: rec.category.clone(),
                        kind: "partial".into(),
                        detail: format!(
                            "{ety} {}..{} covered by [{}], {uncovered} bytes open",
                            exp.start,
                            exp.end,
                            covered.join(", ")
                        ),
                    });
                }
            }
        }
        let mut record_fp = false;
        for (i, p) in predicted.iter().enumerate() {
            if !matched_pred[i] {
                let pty = canonical_type(p.ty.as_str()).to_string();
                // A prediction fully inside an expected span of another type is a labelling nuance, not a leak.
                let inside_expected = rec
                    .entities
                    .iter()
                    .any(|e| e.start <= p.start && p.end <= e.end);
                if inside_expected {
                    continue;
                }
                by_type.entry(pty.clone()).or_default().fp += 1;
                micro.fp += 1;
                record_fp = true;
                if failures.len() < max_failures {
                    failures.push(Failure {
                        id: rec.id.clone(),
                        category: rec.category.clone(),
                        kind: "false_positive".into(),
                        detail: format!(
                            "{pty} predicted at {}..{} ({})",
                            p.start, p.end, p.evidence
                        ),
                    });
                }
            }
        }
        if record_leaked {
            leaked_records += 1;
        }
        if record_fp {
            false_positive_records += 1;
        }
        if rec.category == "NEGATIVE" {
            negative_records += 1;
            if predicted.is_empty() {
                negative_clean += 1;
            }
        }
        similarity_sum += similarity(&result.masked, &rec.masked);
        if engine.demask(&result.masked, &result.entries, true) == rec.text {
            demask_exact += 1;
        } else if failures.len() < max_failures {
            failures.push(Failure {
                id: rec.id.clone(),
                category: rec.category.clone(),
                kind: "demask".into(),
                detail: "demask(mask(text)) != text".into(),
            });
        }
    }

    Report {
        records: records.len(),
        by_type,
        micro,
        leak_chars,
        expected_chars,
        leaked_records,
        false_positive_records,
        negative_records,
        negative_clean,
        mask_similarity: if records.is_empty() {
            1.0
        } else {
            similarity_sum / records.len() as f64
        },
        demask_exact,
        failures,
    }
}

pub fn render_markdown(report: &Report, title: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {title}\n\n"));
    s.push_str(&format!("Записей: {}\n\n", report.records));
    s.push_str(
        "| Тип | TP | FP | FN | Precision | Recall | F1 |\n|---|---:|---:|---:|---:|---:|---:|\n",
    );
    for (ty, st) in &report.by_type {
        s.push_str(&format!(
            "| {ty} | {} | {} | {} | {:.3} | {:.3} | {:.3} |\n",
            st.tp,
            st.fp,
            st.fn_,
            st.precision(),
            st.recall(),
            st.f1()
        ));
    }
    s.push_str(&format!(
        "| **Всего (micro)** | {} | {} | {} | **{:.3}** | **{:.3}** | **{:.3}** |\n\n",
        report.micro.tp,
        report.micro.fp,
        report.micro.fn_,
        report.micro.precision(),
        report.micro.recall(),
        report.micro.f1()
    ));
    let leak_rate = if report.expected_chars == 0 {
        0.0
    } else {
        report.leak_chars as f64 / report.expected_chars as f64
    };
    s.push_str(&format!("- Утечка (символы ПДн, оставшиеся незамаскированными): {:.3}% ({} из {}), записей с утечкой: {}\n", leak_rate * 100.0, report.leak_chars, report.expected_chars, report.leaked_records));
    s.push_str(&format!(
        "- Записей с ложными срабатываниями: {} из {}\n",
        report.false_positive_records, report.records
    ));
    s.push_str(&format!(
        "- Ловушки без ПДн (NEGATIVE) чистые: {} из {}\n",
        report.negative_clean, report.negative_records
    ));
    s.push_str(&format!(
        "- Сходство замаскированного текста с эталоном (1 − нормированный Левенштейн): {:.4}\n",
        report.mask_similarity
    ));
    s.push_str(&format!(
        "- Точное демаскирование demask(mask(x)) == x: {} из {}\n",
        report.demask_exact, report.records
    ));
    if !report.failures.is_empty() {
        s.push_str(
            "\n## Примеры расхождений\n\n| id | категория | вид | детали |\n|---|---|---|---|\n",
        );
        for f in &report.failures {
            s.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                f.id, f.category, f.kind, f.detail
            ));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert!((similarity("abc", "abc") - 1.0).abs() < 1e-9);
    }
}
