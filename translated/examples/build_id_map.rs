//! Fills the `ids` of every dictionary record ([`Translation`]): the game's text
//! IDs whose English is exactly that record's `en`.
//!
//! Allows translating with the game in any language: the ID is the same in all of them.
//!
//! ```text
//! cargo run --release --example build_id_map -- <lang0_files.txt> <strs folder> <decrypted.jsonl> <dictionary.jsonl>
//! ```
//!
//! The dictionary is rewritten in place; existing `ids` are replaced, so a patch
//! that moves or reuses an ID is picked up. The `textId` comes from
//! `lang0[i] = fileId` as `i * 1024 + entry index`. Plain entries are read from
//! the English `strs`; encrypted ones come from `decrypted.jsonl` (already
//! decrypted in-game). Records whose translation is [`unusable`] get no IDs.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use gw2translated::dict::{NULL_MARKER, unusable};
use gw2translated::rc4;
use gw2translated::records::{self, Decrypted, Translation};

const PER_FILE: u32 = 1024;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [lang0, strs_dir, decrypted, dictionary] = a.as_slice() else {
        eprintln!("usage: build_id_map <lang0_files.txt> <strs folder> <decrypted.jsonl> <dictionary.jsonl>");
        std::process::exit(2);
    };
    let dictionary = Path::new(dictionary);
    let read = records::read::<Translation>(dictionary).unwrap();
    if read.skipped > 0 {
        eprintln!("{dictionary:?}: {} unreadable lines; not rewriting it", read.skipped);
        std::process::exit(1);
    }
    let mut dict = read.records;

    let mut english: BTreeMap<u32, String> = BTreeMap::new();
    let (mut files, mut missing) = (0, 0);
    for (i, line) in std::fs::read_to_string(lang0).unwrap().split_whitespace().enumerate() {
        let Ok(buf) = std::fs::read(Path::new(strs_dir).join(format!("{line}.bin"))) else {
            missing += 1;
            continue;
        };
        if buf.get(..4) != Some(b"strs") {
            continue;
        }
        files += 1;
        let mut pos = 4;
        let mut idx = 0u32;
        while let Some(h) = buf.get(pos..).and_then(rc4::Header::parse) {
            let entry = &buf[pos..pos + h.size as usize];
            if !h.is_protected()
                && let Some(t) = rc4::decode(entry, None).filter(|t| !t.is_empty())
            {
                english.insert(i as u32 * PER_FILE + idx, t);
            }
            pos += h.size as usize;
            idx += 1;
        }
    }
    let plain = english.len();

    for Decrypted { id, en } in records::read::<Decrypted>(Path::new(decrypted)).unwrap().records {
        if !en.is_empty() {
            english.entry(id).or_insert(en);
        }
    }

    let mut by_en: HashMap<&str, usize> = HashMap::new();
    for (i, t) in dict.iter().enumerate() {
        by_en.entry(t.en.as_str()).or_insert(i);
    }
    let mut ids: Vec<Vec<u32>> = vec![Vec::new(); dict.len()];
    for (&id, en) in &english {
        // `[null]` is the game's marker for an empty string; the ID may be reused in a patch.
        if en.contains(NULL_MARKER) {
            continue;
        }
        if let Some(&i) = by_en.get(en.as_str()) {
            ids[i].push(id); // `english` is ordered by ID, so each list is sorted and unique
        }
    }
    let (mut with_ids, mut mapped) = (0, 0);
    for (t, ids) in dict.iter_mut().zip(ids) {
        t.ids = if unusable(&t.en, &t.tr) { Vec::new() } else { ids };
        with_ids += usize::from(!t.ids.is_empty());
        mapped += t.ids.len();
    }
    records::write_all(dictionary, &dict).unwrap();
    println!(
        "files={files} (missing {missing}) english_ids={} (plain {plain}, encrypted {}) records_with_ids={with_ids}/{} ids={mapped}",
        english.len(),
        english.len() - plain,
        dict.len()
    );
}
