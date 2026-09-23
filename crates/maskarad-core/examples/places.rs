//! Prints gazetteer hits for a text.
use maskarad_core::detect::gazetteer::Gazetteer;
use maskarad_core::inflect::place_forms;
use maskarad_core::normalize::Normalized;
use maskarad_core::Dictionaries;

fn main() {
    let text = std::env::args().nth(1).unwrap_or_default();
    let dict = Dictionaries::builtin();
    let ru: Vec<&str> = dict
        .places
        .iter()
        .filter(|p| p.name.contains("росси"))
        .map(|p| p.name.as_str())
        .collect();
    println!("entries with росси: {ru:?}");
    println!(
        "forms of 'российская федерация': {:?}",
        place_forms("российская федерация")
    );
    println!("country_forms: {}", dict.country_forms.len());
    let g = Gazetteer::build(&dict).unwrap();
    let norm = Normalized::new(&text);
    for h in g.find(&norm.lower) {
        println!("{:?} {:?}", h.kind, &norm.lower[h.start..h.end]);
    }
}
