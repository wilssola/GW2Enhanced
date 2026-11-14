//! Remembering where sites were found, per build of the game.
//!
//! Same build (same fingerprint): the stored addresses are used after a cheap re-check, so start-up skips the
//! scan. New build: the cache is ignored and rebuilt. The cache is a speed-up and a record, never trusted blindly:
//! a signature site is verified again at the stored address before it is used.

use std::collections::BTreeMap;
use std::path::Path;

use crate::module::Module;
use crate::pattern::Pattern;
use crate::resolve::{Located, ResolveError, looks_like_function_start, resolve};
use crate::site::{Site, Strategy};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cache {
    pub fingerprint: u64,
    /// site name -> (rva, strategy index)
    pub entries: BTreeMap<String, (u32, usize)>,
}

impl Cache {
    pub fn new(fingerprint: u64) -> Self {
        Self {
            fingerprint,
            entries: BTreeMap::new(),
        }
    }

    pub fn to_text(&self) -> String {
        let mut s = format!("fingerprint={:016x}\n", self.fingerprint);
        for (name, (rva, strategy)) in &self.entries {
            s.push_str(&format!("{name} {rva:#x} {strategy}\n"));
        }
        s
    }

    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        let fp = lines.next()?.strip_prefix("fingerprint=")?;
        let mut c = Self::new(u64::from_str_radix(fp.trim(), 16).ok()?);
        for l in lines.filter(|l| !l.trim().is_empty()) {
            let mut it = l.split_whitespace();
            let (name, rva, strategy) = (it.next()?, it.next()?, it.next()?);
            let rva = u32::from_str_radix(rva.strip_prefix("0x")?, 16).ok()?;
            c.entries.insert(name.to_string(), (rva, strategy.parse().ok()?));
        }
        Some(c)
    }

    pub fn load(path: &Path) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(path).ok()?)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.to_text())
    }

    /// Use for a module: the stored entries when the build is the same, an empty cache otherwise.
    pub fn for_module(self, m: &Module) -> Self {
        let fp = m.fingerprint();
        if self.fingerprint == fp { self } else { Self::new(fp) }
    }
}

/// Like [`resolve`], but tries the cache first and records what it found.
pub fn resolve_cached(m: &Module, site: &Site, cache: &mut Cache) -> Result<Located, ResolveError> {
    if let Some(&(rva, idx)) = cache.entries.get(site.name)
        && let Some(st) = site.strategies.get(idx)
        && still_valid(m, st, rva)
    {
        return Ok(Located {
            rva,
            strategy: idx,
            method: st.method(),
            looks_like_start: looks_like_function_start(m, rva),
        });
    }
    let l = resolve(m, site)?;
    cache.entries.insert(site.name.to_string(), (l.rva, l.strategy));
    Ok(l)
}

/// A cheap re-check of a stored address.
fn still_valid(m: &Module, st: &Strategy, rva: u32) -> bool {
    match st {
        Strategy::Sig(s) => Pattern::parse(s).is_some_and(|p| {
            m.code_at(rva, p.len())
                .is_some_and(|c| c.len() == p.len() && p.find_all(c).first() == Some(&0))
        }),
        // anchored on a string: nothing cheap to check beyond being inside the code
        Strategy::StringXref(_) | Strategy::CalleeOfAnchor(..) => m.contains_code(rva),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::testutil::fake_pe;
    use crate::site::{Entry, Risk};

    fn code() -> Vec<u8> {
        let mut c = vec![0xCC; 8];
        c.extend_from_slice(&[0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0xC3]);
        c
    }

    const SITE: Site = Site::new(
        "f",
        &[Strategy::Sig("40 53 48 83 EC 20")],
        Entry::FunctionStart,
        Risk::Low,
    );

    #[test]
    fn text_round_trip() {
        let mut c = Cache::new(0xABCD);
        c.entries.insert("a".into(), (0x1010, 0));
        c.entries.insert("b".into(), (0x2020, 2));
        assert_eq!(Cache::parse(&c.to_text()), Some(c));
        assert!(Cache::parse("garbage").is_none());
    }

    #[test]
    fn records_and_reuses() {
        let f = fake_pe(&code(), b"");
        let m = Module::from_file_bytes(&f).unwrap();
        let mut cache = Cache::new(m.fingerprint());
        let first = resolve_cached(&m, &SITE, &mut cache).unwrap();
        assert_eq!(cache.entries["f"], (first.rva, 0));
        let again = resolve_cached(&m, &SITE, &mut cache).unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn a_stale_entry_is_not_trusted() {
        let f = fake_pe(&code(), b"");
        let m = Module::from_file_bytes(&f).unwrap();
        let mut cache = Cache::new(m.fingerprint());
        cache.entries.insert("f".into(), (0x1003, 0)); // wrong address
        let l = resolve_cached(&m, &SITE, &mut cache).unwrap();
        assert_eq!(l.rva, 0x1008, "falls back to a real search");
        assert_eq!(cache.entries["f"], (0x1008, 0), "and fixes the entry");
    }

    #[test]
    fn another_build_drops_the_cache() {
        let a = fake_pe(&code(), b"");
        let mut other = code();
        other.push(0x90);
        let b = fake_pe(&other, b"");
        let (ma, mb) = (
            Module::from_file_bytes(&a).unwrap(),
            Module::from_file_bytes(&b).unwrap(),
        );
        let mut c = Cache::new(ma.fingerprint());
        c.entries.insert("f".into(), (0x1008, 0));
        assert!(c.clone().for_module(&ma).entries.contains_key("f"));
        assert!(c.for_module(&mb).entries.is_empty());
    }
}
