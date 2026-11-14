//! Merges several `decrypted.jsonl` files ([`Decrypted`]) into one.
//!
//! The RC4 keys come from the server and are the same for every player: one
//! player's collection works for everyone. The first file wins on conflicts.
//!
//! ```text
//! cargo run --release --example merge_decrypted -- <output.jsonl> <decrypted.jsonl>...
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use gw2translated::records::{self, Decrypted};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(out), inputs) = (args.next(), args.collect::<Vec<_>>()) else {
        eprintln!("usage: merge_decrypted <output.jsonl> <decrypted.jsonl>...");
        std::process::exit(2);
    };
    let mut all: BTreeMap<u32, String> = BTreeMap::new();
    let mut conflicts = 0;
    for path in &inputs {
        let r = records::read::<Decrypted>(Path::new(path)).unwrap();
        let (mut n, mut new) = (0, 0);
        for Decrypted { id, en } in r.records {
            if en.is_empty() {
                continue;
            }
            n += 1;
            match all.get(&id) {
                None => {
                    all.insert(id, en);
                    new += 1;
                }
                Some(t) if *t != en => conflicts += 1,
                Some(_) => {}
            }
        }
        println!("{path}: {n} ids, {new} new, {} unreadable lines", r.skipped);
    }
    let merged: Vec<Decrypted> = all.into_iter().map(|(id, en)| Decrypted { id, en }).collect();
    records::write_all(Path::new(&out), &merged).unwrap();
    println!(
        "total: {} ids ({conflicts} conflicts, kept the first) -> {out}",
        merged.len()
    );
}
