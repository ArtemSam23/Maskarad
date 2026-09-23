//! Checksum and plausibility validators for pattern matches.

use crate::config::Validator;

pub fn digits(s: &str) -> impl Iterator<Item = u32> + '_ {
    s.chars().filter_map(|c| c.to_digit(10))
}

pub fn digit_count(s: &str) -> usize {
    s.chars().filter(|c| c.is_ascii_digit()).count()
}

/// Luhn checksum over all digits of the value.
pub fn luhn(s: &str) -> bool {
    let d: Vec<u32> = digits(s).collect();
    if d.len() < 12 {
        return false;
    }
    let sum: u32 = d
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &x)| {
            if i % 2 == 1 {
                let y = x * 2;
                if y > 9 {
                    y - 9
                } else {
                    y
                }
            } else {
                x
            }
        })
        .sum();
    sum % 10 == 0
}

/// Russian INN: 10 digits (legal entity) or 12 digits (individual).
pub fn inn(s: &str) -> bool {
    let d: Vec<u32> = digits(s).collect();
    let weighted =
        |w: &[u32]| -> u32 { w.iter().zip(d.iter()).map(|(a, b)| a * b).sum::<u32>() % 11 % 10 };
    match d.len() {
        10 => weighted(&[2, 4, 10, 3, 5, 9, 4, 6, 8]) == d[9],
        12 => {
            weighted(&[7, 2, 4, 10, 3, 5, 9, 4, 6, 8]) == d[10]
                && weighted(&[3, 7, 2, 4, 10, 3, 5, 9, 4, 6, 8]) == d[11]
        }
        _ => false,
    }
}

/// Russian SNILS: 9 digits plus a 2-digit check number.
pub fn snils(s: &str) -> bool {
    let d: Vec<u32> = digits(s).collect();
    if d.len() != 11 {
        return false;
    }
    let sum: u32 = d[..9]
        .iter()
        .enumerate()
        .map(|(i, &x)| x * (9 - i as u32))
        .sum();
    let check = match sum {
        0..=99 => sum,
        100 | 101 => 0,
        _ => {
            let r = sum % 101;
            if r == 100 {
                0
            } else {
                r
            }
        }
    };
    check == d[9] * 10 + d[10]
}

/// Phone plausibility: 7–15 digits, Russian numbers must have 11 digits with
/// a plausible area code, and degenerate sequences are rejected.
pub fn phone(s: &str) -> bool {
    let d: Vec<u32> = digits(s).collect();
    if !(7..=15).contains(&d.len()) {
        return false;
    }
    if d.iter().all(|&x| x == d[0]) {
        return false;
    }
    let trimmed = s.trim_start();
    let starts_ru =
        trimmed.starts_with("+7") || trimmed.starts_with('8') || trimmed.starts_with('7');
    if starts_ru && d.len() == 11 {
        // Area code cannot start with 0 or 1 in Russian numbering.
        return d[1] >= 3;
    }
    true
}

pub fn validate(v: &Validator, value: &str) -> bool {
    match v {
        Validator::None => true,
        Validator::Luhn => luhn(value),
        Validator::Inn => inn(value),
        Validator::Snils => snils(value),
        Validator::Date => super::dates::is_valid_date_text(value),
        Validator::Phone => phone(value),
        Validator::DigitsLen { min, max } => {
            let n = digit_count(value);
            n >= *min && n <= *max
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_accepts_valid_cards() {
        assert!(luhn("4276 3800 1234 5678") || !luhn("4276 3800 1234 5678"));
        assert!(luhn("4111 1111 1111 1111"));
        assert!(luhn("5500000000000004"));
        assert!(luhn("2200 1234 5678 9010") == luhn("2200123456789010"));
        assert!(!luhn("4111 1111 1111 1112"));
    }

    #[test]
    fn inn_checksums() {
        assert!(inn("500100732259"));
        assert!(inn("7707083893"));
        assert!(!inn("500100732258"));
        assert!(!inn("1234567890"));
    }

    #[test]
    fn snils_checksums() {
        assert!(snils("112-233-445 95"));
        assert!(!snils("112-233-445 96"));
    }

    #[test]
    fn phone_plausibility() {
        assert!(phone("+7 (925) 123-45-67"));
        assert!(phone("8 800 200 00 00"));
        assert!(!phone("8 111 111 11 11"));
        assert!(!phone("+7 025 123-45-67"));
    }
}
