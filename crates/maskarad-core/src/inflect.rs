//! Light-weight Russian inflection rules for names and place names.
//!
//! Dictionaries hold the nominative form; these helpers generate the case
//! forms a text may contain (`Иван` → `Ивана`, `Ивану`, …) and reduce an
//! inflected surname back to candidate nominative lemmas. The rules are
//! deliberately approximate: over-generation only costs a few extra
//! dictionary entries, under-generation costs recall.

/// Case forms of a first name (folded, lower case, `ё` → `е`).
pub fn first_name_forms(name: &str) -> Vec<String> {
    let n = name;
    let mut out = vec![n.to_string()];
    let push = |out: &mut Vec<String>, stem: &str, endings: &[&str]| {
        for e in endings {
            out.push(format!("{stem}{e}"));
        }
    };
    if let Some(stem) = n.strip_suffix("ия") {
        push(&mut out, &format!("{stem}и"), &["и", "ю", "ей", "ею", "е"]);
    } else if let Some(stem) = n.strip_suffix('а') {
        let soft = ["к", "г", "х", "ш", "ч", "ж", "щ"]
            .iter()
            .any(|s| stem.ends_with(s));
        push(
            &mut out,
            stem,
            if soft {
                &["и", "е", "у", "ой", "ою"]
            } else {
                &["ы", "е", "у", "ой", "ою"]
            },
        );
    } else if let Some(stem) = n.strip_suffix('я') {
        push(&mut out, stem, &["и", "е", "ю", "ей", "ею"]);
    } else if let Some(stem) = n.strip_suffix('й') {
        push(&mut out, stem, &["я", "ю", "ем", "е"]);
    } else if let Some(stem) = n.strip_suffix('ь') {
        push(&mut out, stem, &["я", "ю", "ем", "е", "и", "ью"]);
    } else if let Some(stem) = n.strip_suffix("ел") {
        // Павел → Павла (fleeting vowel), keep the regular forms as well.
        push(&mut out, &format!("{stem}л"), &["а", "у", "ом", "е"]);
        push(&mut out, n, &["а", "у", "ом", "е"]);
    } else if n.ends_with(|c: char| "бвгджзклмнпрстфхцчшщ".contains(c)) {
        push(&mut out, n, &["а", "у", "ом", "е", "ем"]);
    }
    if n == "лев" {
        push(&mut out, "льв", &["а", "у", "ом", "е"]);
    }
    out
}

/// Case forms of a patronymic.
pub fn patronymic_forms(p: &str) -> Vec<String> {
    let mut out = vec![p.to_string()];
    if p.ends_with("ич") {
        for e in ["а", "у", "ем", "е"] {
            out.push(format!("{p}{e}"));
        }
    } else if let Some(stem) = p.strip_suffix("на") {
        for e in ["ны", "не", "ну", "ной", "ною"] {
            out.push(format!("{stem}{e}"));
        }
    }
    out
}

/// Whether a token looks like a Russian patronymic in any case or gender.
pub fn is_patronymic_like(t: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "ович",
        "овича",
        "овичу",
        "овичем",
        "овиче",
        "евич",
        "евича",
        "евичу",
        "евичем",
        "евиче",
        "овна",
        "овны",
        "овне",
        "овну",
        "овной",
        "евна",
        "евны",
        "евне",
        "евну",
        "евной",
        "ична",
        "ичны",
        "ичне",
        "ичну",
        "ичной",
        "инична",
        "иничны",
        "иничне",
        "иничну",
        "иничной",
    ];
    if t.chars().count() < 5 {
        return false;
    }
    if SUFFIXES.iter().any(|s| t.ends_with(s)) {
        return true;
    }
    // Short -ич patronymics: Ильич, Кузьмич, Лукич, Фомич, Никитич, Саввич, Яковлевич is covered above.
    const SHORT: &[&str] = &[
        "ильич",
        "кузьмич",
        "лукич",
        "фомич",
        "никитич",
        "саввич",
        "мич",
    ];
    SHORT.iter().any(|s| t.starts_with(s) || t == *s)
        && ["ич", "ича", "ичу", "ичем", "иче"]
            .iter()
            .any(|e| t.ends_with(e))
}

/// Candidate nominative (male) lemmas for a surname token in any case/gender.
/// The token itself is always the first candidate.
pub fn surname_lemmas(t: &str) -> Vec<String> {
    let mut out = vec![t.to_string()];
    let mut add = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };
    let rules: &[(&str, &[&str])] = &[
        // -ов/-ев/-ин/-ын family (female and oblique cases)
        ("ова", &["ов"]),
        ("ову", &["ов"]),
        ("овым", &["ов"]),
        ("ове", &["ов"]),
        ("овой", &["ов"]),
        ("овых", &["ов"]),
        ("овыми", &["ов"]),
        ("ева", &["ев"]),
        ("еву", &["ев"]),
        ("евым", &["ев"]),
        ("еве", &["ев"]),
        ("евой", &["ев"]),
        ("евых", &["ев"]),
        ("евыми", &["ев"]),
        ("ина", &["ин"]),
        ("ину", &["ин"]),
        ("иным", &["ин"]),
        ("ине", &["ин"]),
        ("иной", &["ин"]),
        ("иных", &["ин"]),
        ("иными", &["ин"]),
        ("ына", &["ын"]),
        ("ыну", &["ын"]),
        ("ыным", &["ын"]),
        ("ыне", &["ын"]),
        ("ыной", &["ын"]),
        ("ыных", &["ын"]),
        // -ский/-цкий family
        ("ского", &["ский"]),
        ("скому", &["ский"]),
        ("ским", &["ский"]),
        ("ском", &["ский"]),
        ("ская", &["ский"]),
        ("ской", &["ский"]),
        ("скую", &["ский"]),
        ("ских", &["ский"]),
        ("скими", &["ский"]),
        ("цкого", &["цкий"]),
        ("цкому", &["цкий"]),
        ("цким", &["цкий"]),
        ("цком", &["цкий"]),
        ("цкая", &["цкий"]),
        ("цкой", &["цкий"]),
        ("цкую", &["цкий"]),
        ("цких", &["цкий"]),
        // adjectival surnames (Толстой, Полевой, Белый, Чёрный)
        ("ого", &["ой", "ый", "ий"]),
        ("ому", &["ой", "ый", "ий"]),
        ("ая", &["ой", "ый", "ий"]),
        ("ую", &["ой", "ый", "ий"]),
        ("ым", &["ый", "ой"]),
        ("им", &["ий"]),
        ("ых", &["ых", "ый", "ой"]),
        ("их", &["их", "ий"]),
        // consonant-final surnames in oblique cases (Мицкевича, Шмидту, Бондарем)
        ("а", &[""]),
        ("у", &[""]),
        ("ом", &[""]),
        ("ем", &[""]),
        ("е", &[""]),
        ("я", &["ь", "й"]),
        ("ю", &["ь", "й"]),
        ("и", &["ь", "а", "я"]),
    ];
    for (suffix, replacements) in rules {
        if let Some(stem) = t.strip_suffix(suffix) {
            if stem.chars().count() < 2 {
                continue;
            }
            for r in *replacements {
                add(format!("{stem}{r}"));
            }
        }
    }
    out
}

/// Whether a token has the shape of a Russian surname (any case or gender).
pub fn surname_like(t: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "ов",
        "ова",
        "ову",
        "овым",
        "ове",
        "овой",
        "овых",
        "овыми",
        "ев",
        "ева",
        "еву",
        "евым",
        "еве",
        "евой",
        "евых",
        "евыми",
        "ин",
        "ина",
        "ину",
        "иным",
        "ине",
        "иной",
        "иных",
        "ын",
        "ына",
        "ыну",
        "ыным",
        "ыне",
        "ыной",
        "ский",
        "ская",
        "ского",
        "скому",
        "ским",
        "ском",
        "ской",
        "скую",
        "ских",
        "скими",
        "цкий",
        "цкая",
        "цкого",
        "цкому",
        "цким",
        "цком",
        "цкой",
        "цкую",
        "цких",
        "ых",
        "их",
        "ко",
        "енко",
        "ук",
        "юк",
        "чук",
        "як",
        "ян",
        "яна",
        "яну",
        "яном",
        "яне",
        "дзе",
        "швили",
        "ич",
        "ича",
        "ичу",
        "ичем",
        "иче",
        "ец",
        "ца",
        "цу",
        "цем",
        "ер",
        "ера",
        "еру",
        "ером",
        "ман",
        "мана",
        "ману",
        "маном",
    ];
    let len = t.chars().count();
    if len < 4 {
        return false;
    }
    SUFFIXES
        .iter()
        .any(|s| t.ends_with(s) && len > s.chars().count() + 1)
}

/// Case forms of a place name (city, region, country). Multi-word names are
/// inflected word by word and combined.
pub fn place_forms(name: &str) -> Vec<String> {
    let words: Vec<&str> = name.split(' ').collect();
    let per_word: Vec<Vec<String>> = words.iter().map(|w| word_place_forms(w)).collect();
    let mut combos: Vec<String> = vec![String::new()];
    for (i, forms) in per_word.iter().enumerate() {
        let mut next = Vec::with_capacity(combos.len() * forms.len());
        for prefix in &combos {
            for f in forms {
                let mut s = prefix.clone();
                if i > 0 {
                    s.push(' ');
                }
                s.push_str(f);
                next.push(s);
            }
        }
        combos = next;
    }
    combos
}

fn word_place_forms(w: &str) -> Vec<String> {
    let mut out = vec![w.to_string()];
    let mut push = |stem: &str, endings: &[&str]| {
        for e in endings {
            out.push(format!("{stem}{e}"));
        }
    };
    // Hyphenated names (Санкт-Петербург, Ростов-на-Дону) inflect the last part.
    if let Some((head, tail)) = w.rsplit_once('-') {
        if tail.chars().count() > 3 && head != "ростов" {
            for f in word_place_forms(tail).into_iter().skip(1) {
                out.push(format!("{head}-{f}"));
            }
            return out;
        }
    }
    if w.chars().count() < 3 {
        return out;
    }
    if let Some(stem) = w.strip_suffix("ия") {
        push(&format!("{stem}и"), &["и", "ю", "ей", "ею"]);
    } else if let Some(stem) = w.strip_suffix("ые") {
        push(stem, &["ых", "ым", "ыми"]);
    } else if let Some(stem) = w.strip_suffix("ие") {
        push(stem, &["их", "им", "ими"]);
    } else if let Some(stem) = w.strip_suffix("ий") {
        push(stem, &["его", "ему", "им", "ем"]);
    } else if let Some(stem) = w.strip_suffix("ый") {
        push(stem, &["ого", "ому", "ым", "ом"]);
    } else if let Some(stem) = w.strip_suffix("ой") {
        push(stem, &["ого", "ому", "ым", "ом"]);
    } else if let Some(stem) = w.strip_suffix("ая") {
        push(stem, &["ой", "ую", "ою"]);
    } else if let Some(stem) = w.strip_suffix("яя") {
        push(stem, &["ей", "юю", "ею"]);
    } else if let Some(stem) = w.strip_suffix('а') {
        let soft = ["к", "г", "х", "ш", "ч", "ж", "щ"]
            .iter()
            .any(|s| stem.ends_with(s));
        push(
            stem,
            if soft {
                &["и", "е", "у", "ой", "ою"]
            } else {
                &["ы", "е", "у", "ой", "ою"]
            },
        );
    } else if let Some(stem) = w.strip_suffix('я') {
        push(stem, &["и", "е", "ю", "ей"]);
    } else if let Some(stem) = w.strip_suffix('ы') {
        push(stem, &["ов", "ам", "ами", "ах"]);
    } else if let Some(stem) = w.strip_suffix('ь') {
        push(stem, &["и", "ю", "ем", "ью", "я"]);
    } else if let Some(stem) = w.strip_suffix('й') {
        push(stem, &["я", "ю", "ем", "е"]);
    } else if w.ends_with(|c: char| "бвгджзклмнпрстфхцчшщ".contains(c)) {
        push(w, &["а", "у", "ом", "е"]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_name_forms_cover_common_cases() {
        let f = first_name_forms("иван");
        for x in ["иван", "ивана", "ивану", "иваном", "иване"] {
            assert!(f.contains(&x.to_string()), "{x}");
        }
        let f = first_name_forms("мария");
        assert!(f.contains(&"марии".to_string()));
        assert!(f.contains(&"марией".to_string()));
        let f = first_name_forms("павел");
        assert!(f.contains(&"павла".to_string()));
        assert!(first_name_forms("лев").contains(&"льва".to_string()));
    }

    #[test]
    fn surname_lemmas_reduce_to_nominative() {
        assert!(surname_lemmas("ивановой").contains(&"иванов".to_string()));
        assert!(surname_lemmas("ивановым").contains(&"иванов".to_string()));
        assert!(surname_lemmas("достоевского").contains(&"достоевский".to_string()));
        assert!(surname_lemmas("толстого").contains(&"толстой".to_string()));
        assert!(surname_lemmas("мицкевича").contains(&"мицкевич".to_string()));
    }

    #[test]
    fn patronymic_shapes() {
        assert!(is_patronymic_like("иванович"));
        assert!(is_patronymic_like("ивановне"));
        assert!(is_patronymic_like("ильинична"));
        assert!(is_patronymic_like("ильича"));
        assert!(!is_patronymic_like("москвич"));
        assert!(!is_patronymic_like("кирпич"));
    }

    #[test]
    fn place_forms_inflect_multiword() {
        let f = place_forms("нижний новгород");
        assert!(f.contains(&"нижнем новгороде".to_string()));
        assert!(f.contains(&"нижнего новгорода".to_string()));
        assert!(place_forms("москва").contains(&"москве".to_string()));
        assert!(place_forms("казань").contains(&"казани".to_string()));
        assert!(place_forms("санкт-петербург").contains(&"санкт-петербурге".to_string()));
    }
}
