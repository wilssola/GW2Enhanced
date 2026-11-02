//! Background collection: `discovered.jsonl` and `decrypted.jsonl` (see
//! [`crate::records`]).
//!
//! - `discovered.jsonl`: English game texts seen without a translation.
//! - `decrypted.jsonl`: protected (RC4) strings with their ID, decrypted with
//!   the `keySeed` the server sent.
//!
//! `seed_decode` matches the encrypted entries the game puts in the cache
//! (`reg-insert`) with the `keySeed`s the server sends (`bulk_insert`) and
//! decrypts each pair as soon as both sides arrive, whether or not the text
//! shows up on screen.
//!
//! The hook only does `send`; deduplication, RC4 and disk I/O stay on this thread.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use crate::dict::NULL_MARKER;
use crate::records::{self, Appender, Decrypted, Discovered};
use crate::{log, rc4};

pub enum Item {
    /// Displayed text that is not in the dictionary.
    Discovered(String),
    /// Encrypted string, already decrypted by the game.
    Seed(u32, String),
    /// Encrypted `strs` entry the game put in the cache, still without a key.
    Entry(u32, Vec<u8>),
    /// `(textId, keySeed)` pairs coming from the server.
    Keys(Vec<(u32, u64)>),
    /// Lines just shown (speech bubbles, story scenes, any text without a
    /// translation): the lines that usually follow them (next IDs in the same
    /// text file) are translated ahead.
    Upcoming(Vec<u32>),
}

/// Lines after a dialogue line that are translated ahead of time.
const UPCOMING: u32 = 6;
/// Strings per text file (`textId = file * 1024 + index`).
const FILE_SIZE: u32 = 1024;

pub struct Collector {
    tx: Sender<Item>,
}

impl Collector {
    pub fn send(&self, item: Item) {
        let _ = self.tx.send(item);
    }
}

struct Sink {
    seen_en: HashSet<String>,
    /// Every protected string already decrypted, by ID.
    known: HashMap<u32, String>,
    discovered: Appender,
    decrypted: Appender,
    new_en: u64,
    new_ids: u64,
    /// Entries and keys waiting for their pair.
    entries: HashMap<u32, Vec<u8>>,
    keys: HashMap<u32, u64>,
    decoded: u64,
}

impl Sink {
    fn open(discovered: &Path, decrypted: &Path) -> io::Result<Self> {
        let d = records::read::<Discovered>(discovered)?;
        let k = records::read::<Decrypted>(decrypted)?;
        let seen_en: HashSet<String> = d.records.into_iter().map(|r| r.en).collect();
        let known: HashMap<u32, String> = k.records.into_iter().map(|r| (r.id, r.en)).collect();
        log!(
            "collect: {} texts and {} ids already recorded ({} unreadable lines skipped)",
            seen_en.len(),
            known.len(),
            d.skipped + k.skipped
        );
        Ok(Self {
            seen_en,
            known,
            discovered: Appender::open(discovered)?,
            decrypted: Appender::open(decrypted)?,
            new_en: 0,
            new_ids: 0,
            entries: HashMap::new(),
            keys: HashMap::new(),
            decoded: 0,
        })
    }

    fn take(&mut self, item: Item) -> io::Result<()> {
        match item {
            Item::Discovered(en) => {
                let useful = !en.trim().is_empty() && !en.contains(NULL_MARKER);
                if useful && self.seen_en.insert(en.clone()) {
                    self.discovered.write(&Discovered { en })?;
                    self.new_en += 1;
                }
            }
            Item::Seed(id, text) => self.seed(id, text)?,
            Item::Entry(id, bytes) => {
                if !self.known.contains_key(&id) {
                    match self.keys.get(&id) {
                        Some(&key) => self.decrypt(id, &bytes, key)?,
                        None => {
                            self.entries.insert(id, bytes);
                        }
                    }
                }
            }
            Item::Upcoming(ids) => {
                for id in ids {
                    for next in id.saturating_add(1)..=id.saturating_add(UPCOMING) {
                        if next / FILE_SIZE != id / FILE_SIZE {
                            break;
                        }
                        if let Some(text) = self.known.get(&next) {
                            crate::hooks::soon_if_missing(text, next);
                        }
                    }
                }
            }
            Item::Keys(pairs) => {
                for (id, key) in pairs {
                    // The server sends a key right before the string is used: if we
                    // already know the text, translate it now instead of later.
                    if let Some(text) = self.known.get(&id) {
                        crate::hooks::soon_if_missing(text, id);
                    }
                    self.keys.insert(id, key);
                    if let Some(bytes) = self.entries.remove(&id) {
                        self.decrypt(id, &bytes, key)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn seed(&mut self, id: u32, text: String) -> io::Result<()> {
        // `[null]` doesn't mark the ID: if the real text shows up later, it still gets in.
        if text.trim() == NULL_MARKER {
            return Ok(());
        }
        if let std::collections::hash_map::Entry::Vacant(e) = self.known.entry(id) {
            e.insert(text.clone());
            self.decrypted.write(&Decrypted { id, en: text.clone() })?;
            self.new_ids += 1;
            self.entries.remove(&id);
            self.take(Item::Discovered(text))?;
        }
        Ok(())
    }

    fn decrypt(&mut self, id: u32, entry: &[u8], key: u64) -> io::Result<()> {
        match rc4::decode(entry, Some(key)) {
            Some(text) if !text.is_empty() => {
                self.decoded += 1;
                if crate::bubble::waiting_for(id) {
                    // A speech bubble on screen is waiting for this line.
                    crate::hooks::on_screen_if_missing(&text, id);
                } else {
                    crate::hooks::prefetch_if_missing(&text, Some(id));
                }
                self.seed(id, text)
            }
            _ => Ok(()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.discovered.flush()?;
        self.decrypted.flush()
    }
}

fn run(rx: Receiver<Item>, mut sink: Sink) {
    let mut dirty = false;
    loop {
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(item) => {
                if let Err(e) = sink.take(item) {
                    log!("collect: write error: {e}");
                }
                dirty = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) if dirty => {
                if let Err(e) = sink.flush() {
                    log!("collect: error while saving: {e}");
                }
                log!(
                    "collect: +{} texts, +{} ids this session ({} decrypted via RC4; waiting: {} entries without a key, {} keys)",
                    sink.new_en,
                    sink.new_ids,
                    sink.decoded,
                    sink.entries.len(),
                    sink.keys.len()
                );
                dirty = false;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = sink.flush();
}

pub fn start(discovered: PathBuf, decrypted: PathBuf) -> io::Result<Collector> {
    let sink = Sink::open(&discovered, &decrypted)?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("gw2translated-collect".into())
        .spawn(move || run(rx, sink))?;
    Ok(Collector { tx })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p).unwrap().lines().map(str::to_owned).collect()
    }

    #[test]
    fn dedupes_against_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let (d, k) = (dir.path().join("discovered.jsonl"), dir.path().join("decrypted.jsonl"));
        std::fs::write(&d, "{\"en\":\"Ingredient\"}\n").unwrap();
        std::fs::write(&k, "{\"id\":621576,\"en\":\"Ingredient\"}\n").unwrap();
        let mut sink = Sink::open(&d, &k).unwrap();
        for item in [
            Item::Discovered("Ingredient".into()),
            Item::Discovered("Leap, then \"strike\".".into()),
            Item::Discovered("Leap, then \"strike\".".into()),
            Item::Seed(621576, "Ingredient".into()),
            Item::Seed(7, "Line1\nLine2".into()),
        ] {
            sink.take(item).unwrap();
        }
        sink.flush().unwrap();
        assert_eq!(
            lines(&d),
            [
                r#"{"en":"Ingredient"}"#,
                r#"{"en":"Leap, then \"strike\"."}"#,
                r#"{"en":"Line1\nLine2"}"#
            ]
        );
        assert_eq!(
            lines(&k),
            [r#"{"id":621576,"en":"Ingredient"}"#, r#"{"id":7,"en":"Line1\nLine2"}"#]
        );
    }

    #[test]
    fn seed_decode_pairs_entry_with_key_in_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let (d, k) = (dir.path().join("d.jsonl"), dir.path().join("k.jsonl"));
        let mut sink = Sink::open(&d, &k).unwrap();
        let e = |h: &str| {
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                .collect()
        };
        sink.take(Item::Entry(
            611643,
            e("180042000700d2a6247de2f9cf3bb5e66a650413128c05e1"),
        ))
        .unwrap();
        sink.take(Item::Keys(vec![(611643, 0x00001adfb96a5a5f), (1601, 0x1f2333d8237f)]))
            .unwrap();
        sink.take(Item::Entry(1601, e("17004300070046a2fc15d32af9d0a59a9e4481080ebc30")))
            .unwrap();
        sink.flush().unwrap();
        assert_eq!(
            lines(&k),
            [
                r#"{"id":611643,"en":"Second Birthday Gift"}"#,
                r#"{"id":1601,"en":"The Cost of Victory"}"#
            ]
        );
        assert_eq!(
            lines(&d),
            [r#"{"en":"Second Birthday Gift"}"#, r#"{"en":"The Cost of Victory"}"#]
        );
        assert!(sink.entries.is_empty());
    }
}
