//! Exports the strings collected in-game that have no translation yet.
//!
//! ```text
//! cargo run --release --example export_pending -- <game's GW2Translated folder> <dictionary.jsonl> <output.jsonl>
//! ```
//!
//! Reads `discovered.jsonl` and `decrypted.jsonl` and writes `{"en":...}` lines,
//! the format `gw2-translate` accepts in `--input`. Discards what is already in
//! the dictionary and text that is already a translation: the old collector
//! recorded some of the PT-BR it had applied itself, padded with spaces or cut
//! to the original's length.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use version::records::{self, Decrypted, Discovered, Translation};

/// The records of a game file; a missing one only warns.
fn game_file<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    if !path.exists() {
        eprintln!("warning: {} not found", path.display());
    }
    records::read(path).unwrap().records
}

/// Spanish letters and words: collecting during a session with the client in
/// Spanish recorded the text in that language.
fn looks_spanish(t: &str) -> bool {
    const WORDS: &[&str] = &[
        "de", "del", "la", "las", "los", "el", "una", "con", "por", "para", "que", "y", "es", "tu", "su",
    ];
    let hits = t
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| WORDS.contains(&w.to_lowercase().as_str()))
        .count();
    t.contains(['ñ', 'Ñ', '¿', '¡']) || hits >= 2
}

/// Portuguese letters and words that do not appear in the game's English text.
fn looks_portuguese(t: &str) -> bool {
    const WORDS: &[&str] = &[
        "você", "não", "é", "uma", "também", "então", "está", "são", "seu", "sua", "pelo", "pela", "um", "que", "dos",
        "das", "muito", "desses", "isso", "aqui",
    ];
    t.contains(['ã', 'õ', 'ç', 'Ã', 'Õ', 'Ç'])
        || t.split(|c: char| !c.is_alphanumeric())
            .any(|w| WORDS.contains(&w.to_lowercase().as_str()))
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [game, main, out] = a.as_slice() else {
        eprintln!("usage: export_pending <GW2Translated folder> <dictionary.jsonl> <output.jsonl>");
        std::process::exit(2);
    };
    let mut english = HashSet::new();
    let mut portuguese = HashSet::new();
    for t in records::read::<Translation>(Path::new(main)).unwrap().records {
        portuguese.insert(t.tr.trim().to_owned());
        english.insert(t.en);
    }

    let game = Path::new(game);
    let collected = game_file::<Discovered>(&game.join("discovered.jsonl"))
        .into_iter()
        .map(|d| d.en)
        .chain(
            game_file::<Decrypted>(&game.join("decrypted.jsonl"))
                .into_iter()
                .map(|d| d.en),
        );
    let (mut total, mut already, mut pt) = (0, 0, 0);
    let mut pending = BTreeSet::new();
    for text in collected {
        total += 1;
        if text.trim().is_empty() {
            continue;
        }
        if english.contains(&text) || english.contains(text.trim()) {
            already += 1;
        } else if portuguese.contains(text.trim()) {
            pt += 1;
        } else {
            pending.insert(text);
        }
    }
    // The old collector also cut translations that did not fit the original space;
    // those fragments do not match any whole translation.
    let truncated: Vec<String> = pending.iter().filter(|t| looks_portuguese(t)).cloned().collect();
    pt += truncated.len();
    for t in &truncated {
        pending.remove(t);
    }
    let spanish: Vec<String> = pending.iter().filter(|t| looks_spanish(t)).cloned().collect();
    for t in &spanish {
        pending.remove(t);
    }

    let lines: Vec<Discovered> = pending.iter().map(|t| Discovered { en: t.clone() }).collect();
    records::write_all(Path::new(out), &lines).unwrap();
    println!(
        "collected={total} already_translated={already} discarded_pt={pt} discarded_es={} pending={} -> {out}",
        spanish.len(),
        pending.len()
    );
}
