//! Merges new translations ([`Translation`] JSON Lines) into the main dictionary.
//!
//! ```text
//! cargo run --release --example merge_translations -- [--replace] <dictionary.jsonl> <new.jsonl>...
//! ```
//!
//! A pair is only added if its markup matches the English (`<br>`, `<c=…>`, `</c>`,
//! `%str1%`/`%num1%` and the like) and it is not [`unusable`] (`[null]` in place
//! of the original, invented `[...]` fixups). An English text already in the
//! dictionary is kept, unless `--replace` is given. The `ids` of both sides
//! are always united (sorted, no repeats). The rejects are listed for review.

use std::collections::BTreeMap;
use std::path::Path;

use version::dict::{markup_signature, unusable};
use version::records::{self, Translation};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let replace = args.iter().any(|a| a == "--replace");
    args.retain(|a| a != "--replace");
    let Some((main, news)) = args.split_first().filter(|(_, n)| !n.is_empty()) else {
        eprintln!("usage: merge_translations [--replace] <dictionary.jsonl> <new.jsonl>...");
        std::process::exit(2);
    };
    let main = Path::new(main);
    let mut dict: BTreeMap<String, Translation> = BTreeMap::new();
    for t in records::read::<Translation>(main).unwrap().records {
        dict.insert(t.en.clone(), t);
    }
    let before = dict.len();
    let (mut added, mut replaced, mut known, mut rejected) = (0, 0, 0, Vec::new());
    for path in news {
        for mut t in records::read::<Translation>(Path::new(path)).unwrap().records {
            if t.tr.trim().is_empty() {
                continue;
            }
            match dict.get_mut(&t.en) {
                Some(e) if !replace || e.tr == t.tr => {
                    known += 1;
                    unite(&mut e.ids, t.ids);
                }
                _ if markup_signature(&t.en) != markup_signature(&t.tr) || unusable(&t.en, &t.tr) => {
                    rejected.push((t.en, t.tr));
                }
                Some(e) => {
                    replaced += 1;
                    unite(&mut t.ids, std::mem::take(&mut e.ids));
                    *e = t;
                }
                None => {
                    added += 1;
                    unite(&mut t.ids, Vec::new());
                    dict.insert(t.en.clone(), t);
                }
            }
        }
    }
    records::write_all(main, dict.values()).unwrap();
    println!(
        "before={before} added={added} replaced={replaced} already_present={known} rejected={} total={}",
        rejected.len(),
        dict.len()
    );
    for (en, tr) in rejected.iter().take(20) {
        println!("  REJECTED (markup mismatch or unusable): {en:?} -> {tr:?}");
    }
}

/// Adds `more` to `ids`, keeping them sorted and without repeats.
fn unite(ids: &mut Vec<u32>, more: Vec<u32>) {
    ids.extend(more);
    ids.sort_unstable();
    ids.dedup();
}
