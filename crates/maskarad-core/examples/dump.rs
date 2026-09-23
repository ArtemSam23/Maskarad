//! Prints candidates and accepted entities for a text: `cargo run -p maskarad-core --example dump -- "текст"`.
use maskarad_core::{CoreConfig, Engine};

fn main() {
    let text = std::env::args().nth(1).unwrap_or_default();
    let cfg: CoreConfig =
        serde_yaml::from_str("systems:\n  - id: test\n    auth: { anonymous: true }\n").unwrap();
    let engine = Engine::new(&cfg).unwrap();
    println!("--- candidates");
    for c in engine.candidates(&text) {
        println!(
            "{:<22} {:.2} weak={:<5} {:<28} {:?}",
            c.ty.to_string(),
            c.confidence,
            c.weak,
            c.evidence,
            &text[c.span.start..c.span.end]
        );
    }
    println!("--- entities");
    let sys = engine.system("test").unwrap();
    for e in engine.detect(&text, sys) {
        println!(
            "{:<22} {:.2} {:<28} {:?}",
            e.ty.to_string(),
            e.confidence,
            e.evidence,
            &text[e.start..e.end]
        );
    }
}
