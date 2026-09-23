//! Times the pipeline stages on a text file: `cargo run --release -p maskarad-core --example profile -- file.txt`.
use maskarad_core::detect::gazetteer::Gazetteer;
use maskarad_core::detect::tokens::tokenize;
use maskarad_core::normalize::Normalized;
use maskarad_core::{CoreConfig, Dictionaries, Engine};
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("file path");
    let text = std::fs::read_to_string(&path).expect("read file");
    let cfg: CoreConfig =
        serde_yaml::from_str("systems:\n  - id: test\n    auth: { anonymous: true }\n").unwrap();
    let t = Instant::now();
    let engine = Engine::new(&cfg).unwrap();
    println!("engine build: {:?}", t.elapsed());
    let sys = engine
        .system("test")
        .or_else(|| engine.anonymous_system())
        .expect("system");
    println!("text: {} bytes, {} chars", text.len(), text.chars().count());

    let t = Instant::now();
    let norm = Normalized::new(&text);
    println!("normalize: {:?}", t.elapsed());
    let t = Instant::now();
    let words = tokenize(&text, &norm);
    println!("tokenize: {:?} ({} words)", t.elapsed(), words.len());
    let dict = Dictionaries::builtin();
    let gaz = Gazetteer::build(&dict).unwrap();
    let t = Instant::now();
    let places = gaz.find(&norm.lower);
    println!("gazetteer: {:?} ({} hits)", t.elapsed(), places.len());

    for (name, dur, n) in engine.detector_timings(&text) {
        println!("  detector {name:<12} {dur:?} ({n} candidates)");
    }
    let t = Instant::now();
    let cands = engine.candidates(&text);
    println!(
        "all detectors (candidates): {:?} ({} candidates)",
        t.elapsed(),
        cands.len()
    );
    let t = Instant::now();
    let ents = engine.detect(&text, sys);
    println!(
        "detect incl. resolve: {:?} ({} entities)",
        t.elapsed(),
        ents.len()
    );
    let t = Instant::now();
    let r = engine.mask(&text, sys);
    println!(
        "mask (detect+mask): {:?} ({} entries)",
        t.elapsed(),
        r.entries.len()
    );
    let t = Instant::now();
    let back = engine.demask(&r.masked, &r.entries, true);
    println!(
        "demask exact: {:?} (roundtrip ok: {})",
        t.elapsed(),
        back == text
    );
}
