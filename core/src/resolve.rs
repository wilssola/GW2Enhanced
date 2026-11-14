//! Finding a [`Site`] in a [`Module`]: strategies in order, each result validated.

use std::fmt;

use crate::module::Module;
use crate::pattern::Pattern;
use crate::site::{Entry, Site, Strategy};

/// Where a site was found and how much to trust it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub rva: u32,
    /// Index into `Site::strategies` of the strategy that worked (0 = the most exact one).
    pub strategy: usize,
    pub method: &'static str,
    /// The byte before the address is the padding or return that ends the previous function.
    pub looks_like_start: bool,
}

impl Located {
    /// Found by something other than the first (most exact) strategy.
    pub fn degraded(&self) -> bool {
        self.strategy > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub method: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NoStrategies,
    /// Every strategy failed; one entry per strategy tried.
    Failed(Vec<Attempt>),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NoStrategies => write!(f, "no strategies"),
            ResolveError::Failed(a) => {
                let parts: Vec<String> = a.iter().map(|x| format!("{}: {}", x.method, x.reason)).collect();
                write!(f, "{}", parts.join("; "))
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Whether `rva` looks like the first instruction of a function: it follows padding (`int3`, `nop`, zero) or a
/// `ret`, or starts the section.
pub fn looks_like_function_start(m: &Module, rva: u32) -> bool {
    if rva == m.text_rva {
        return true;
    }
    match rva.checked_sub(1).and_then(|p| m.code_at(p, 1)) {
        Some(&[b]) => matches!(b, 0xCC | 0x90 | 0xC3 | 0x00),
        _ => false,
    }
}

fn by_signature(m: &Module, sig: &str) -> Result<u32, String> {
    let p = Pattern::parse(sig).ok_or_else(|| "invalid pattern".to_string())?;
    match p.find_all(m.text).as_slice() {
        [one] => Ok(m.text_rva + *one as u32),
        [] => Err("not found".into()),
        many => Err(format!("ambiguous ({} matches)", many.len())),
    }
}

/// Start of the function containing text offset `at`: the nearest position after a run of `int3` padding.
fn function_start_before(m: &Module, at: usize) -> Option<u32> {
    let lo = at.saturating_sub(0x4000);
    let mut p = at;
    while p > lo {
        if p > 0 && m.text[p - 1] == 0xCC && m.text[p] != 0xCC {
            return Some(m.text_rva + p as u32);
        }
        p -= 1;
    }
    None
}

fn by_string_xref(m: &Module, s: &str) -> Result<u32, String> {
    // 1. the string (NUL-terminated, not the tail of a longer one) in the data sections
    let mut needle = s.as_bytes().to_vec();
    needle.push(0);
    let mut targets = Vec::new();
    for (rva, bytes) in &m.data {
        let mut i = 0;
        while let Some(off) = find_bytes(&bytes[i..], &needle) {
            let at = i + off;
            if at == 0 || bytes[at - 1] == 0 {
                targets.push(rva + at as u32);
            }
            i = at + 1;
        }
    }
    if targets.is_empty() {
        return Err("string not found".into());
    }
    // 2. code that loads its address: `lea reg, [rip + disp32]`
    let t = m.text;
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 7 <= t.len() {
        let rex_ok = matches!(t[i], 0x48 | 0x4C | 0x49 | 0x4D);
        if rex_ok && t[i + 1] == 0x8D && t[i + 2] & 0xC7 == 0x05 {
            let disp = i32::from_le_bytes(t[i + 3..i + 7].try_into().unwrap());
            let next = (m.text_rva as i64) + (i as i64) + 7;
            let target = next + disp as i64;
            if target >= 0
                && targets.contains(&(target as u32))
                && let Some(f) = function_start_before(m, i)
                && !starts.contains(&f)
            {
                starts.push(f);
            }
        }
        i += 1;
    }
    match starts.as_slice() {
        [one] => Ok(*one),
        [] => Err("no code references the string".into()),
        many => Err(format!("referenced from {} functions", many.len())),
    }
}

/// Distinct `call rel32` targets of the function starting at `start`, in order of first call.
fn callees_of(m: &Module, start: u32) -> Vec<u32> {
    let off = (start - m.text_rva) as usize;
    let t = m.text;
    let mut out = Vec::new();
    let mut i = off;
    // the function ends at the first run of int3 padding after its start (bounded)
    let end = (off + 0x4000).min(t.len());
    while i + 5 <= end {
        if t[i] == 0xCC && t[i + 1] == 0xCC && i > off {
            break;
        }
        if t[i] == 0xE8 {
            let disp = i32::from_le_bytes(t[i + 1..i + 5].try_into().unwrap());
            let target = m.text_rva as i64 + i as i64 + 5 + disp as i64;
            if target >= 0 && m.contains_code(target as u32) && !out.contains(&(target as u32)) {
                out.push(target as u32);
            }
        }
        i += 1;
    }
    out
}

fn by_callee(m: &Module, anchor: &str, index: usize) -> Result<u32, String> {
    let f = by_string_xref(m, anchor)?;
    callees_of(m, f)
        .get(index)
        .copied()
        .ok_or_else(|| format!("anchored function has no callee #{index}"))
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let first = needle[0];
    let mut i = 0;
    while let Some(off) = hay.get(i..)?.iter().position(|&b| b == first) {
        let at = i + off;
        if hay.get(at..at + needle.len()) == Some(needle) {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

/// Finds `site`: every strategy in order, the first valid result wins.
pub fn resolve(m: &Module, site: &Site) -> Result<Located, ResolveError> {
    if site.strategies.is_empty() {
        return Err(ResolveError::NoStrategies);
    }
    let mut failed = Vec::new();
    for (idx, st) in site.strategies.iter().enumerate() {
        let found = match st {
            Strategy::Sig(s) => by_signature(m, s),
            Strategy::StringXref(s) => by_string_xref(m, s),
            Strategy::CalleeOfAnchor(s, n) => by_callee(m, s, *n),
        };
        match found {
            Ok(rva) => {
                let start = looks_like_function_start(m, rva);
                if site.entry == Entry::FunctionStart && !start {
                    failed.push(Attempt {
                        method: st.method(),
                        reason: "not at the start of a function".into(),
                    });
                    continue;
                }
                return Ok(Located {
                    rva,
                    strategy: idx,
                    method: st.method(),
                    looks_like_start: start,
                });
            }
            Err(reason) => failed.push(Attempt {
                method: st.method(),
                reason,
            }),
        }
    }
    Err(ResolveError::Failed(failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::testutil::fake_pe;
    use crate::site::Risk;

    // rdata: "assertion failed\0" at 0x3000
    const STR: &str = "assertion failed";

    /// Code: int3 padding, then a function that loads the string address, then padding and a second function.
    fn sample() -> Vec<u8> {
        let mut code = vec![0xCC; 16];
        // function A at 0x1010: sub rsp,0x28 ; lea rcx,[rip+disp] -> 0x3000 ; ret
        let a = code.len();
        code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]);
        let lea_at = code.len();
        let next = 0x1000 + lea_at as i64 + 7;
        let disp = (0x3000i64 - next) as i32;
        code.extend_from_slice(&[0x48, 0x8D, 0x0D]);
        code.extend_from_slice(&disp.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
        assert_eq!(a, 16);
        code.extend_from_slice(&[0xCC; 11]);
        // function B: distinctive bytes
        code.extend_from_slice(&[
            0x40, 0x53, 0x48, 0x81, 0xEC, 0x90, 0x00, 0x00, 0x00, 0x48, 0x8B, 0xD9, 0xC3,
        ]);
        code
    }

    fn module(code: &[u8]) -> Vec<u8> {
        let mut rdata = STR.as_bytes().to_vec();
        rdata.push(0);
        fake_pe(code, &rdata)
    }

    #[test]
    fn first_strategy_wins_when_exact() {
        let f = module(&sample());
        let m = Module::from_file_bytes(&f).unwrap();
        let site = Site::new(
            "b",
            &[
                Strategy::Sig("40 53 48 81 EC 90 00 00 00 48 8B D9"),
                Strategy::StringXref(STR),
            ],
            Entry::FunctionStart,
            Risk::Low,
        );
        let l = resolve(&m, &site).unwrap();
        assert_eq!((l.strategy, l.degraded(), l.looks_like_start), (0, false, true));
        assert_eq!(l.rva, 0x1000 + 16 + 4 + 7 + 5 + 11);
    }

    #[test]
    fn falls_back_to_the_string_when_the_bytes_changed() {
        let f = module(&sample());
        let m = Module::from_file_bytes(&f).unwrap();
        // the exact signature no longer exists (a patch changed the prologue), the string still points to the function
        let site = Site::new(
            "a",
            &[Strategy::Sig("48 83 EC 29 48 8D 0D"), Strategy::StringXref(STR)],
            Entry::FunctionStart,
            Risk::Medium,
        );
        let l = resolve(&m, &site).unwrap();
        assert_eq!((l.strategy, l.degraded(), l.method), (1, true, "string reference"));
        assert_eq!(l.rva, 0x1010);
    }

    #[test]
    fn reports_every_failed_strategy() {
        let f = module(&sample());
        let m = Module::from_file_bytes(&f).unwrap();
        let site = Site::new(
            "gone",
            &[Strategy::Sig("DE AD BE EF"), Strategy::StringXref("not in the game")],
            Entry::Anywhere,
            Risk::Low,
        );
        let e = resolve(&m, &site).unwrap_err();
        let ResolveError::Failed(a) = &e else { panic!() };
        assert_eq!(a.len(), 2);
        assert!(e.to_string().contains("signature: not found") && e.to_string().contains("string not found"));
    }

    #[test]
    fn ambiguous_signature_is_rejected() {
        let mut code = sample();
        code.extend_from_slice(&[0xCC; 3]);
        code.extend_from_slice(&[
            0x40, 0x53, 0x48, 0x81, 0xEC, 0x90, 0x00, 0x00, 0x00, 0x48, 0x8B, 0xD9, 0xC3,
        ]);
        let f = module(&code);
        let m = Module::from_file_bytes(&f).unwrap();
        let site = Site::new("dup", &[Strategy::Sig("40 53 48 81 EC 90")], Entry::Anywhere, Risk::Low);
        assert!(
            resolve(&m, &site)
                .unwrap_err()
                .to_string()
                .contains("ambiguous (2 matches)")
        );
    }

    #[test]
    fn function_start_is_checked_when_required() {
        let f = module(&sample());
        let m = Module::from_file_bytes(&f).unwrap();
        // matches inside function A (the lea), not at a start
        let mid = Site::new("mid", &[Strategy::Sig("48 8D 0D")], Entry::FunctionStart, Risk::Low);
        assert!(resolve(&m, &mid).unwrap_err().to_string().contains("not at the start"));
        let any = Site::new("mid", &[Strategy::Sig("48 8D 0D")], Entry::Anywhere, Risk::Low);
        assert!(!resolve(&m, &any).unwrap().looks_like_start);
    }

    #[test]
    fn callee_of_an_anchored_function() {
        // function A (anchored by the string) calls B at 0x1010+... : build code with a call to function B
        let mut code = vec![0xCC; 16];
        let lea_at = code.len() + 4;
        code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]);
        let disp = (0x3000i64 - (0x1000 + lea_at as i64 + 7)) as i32;
        code.extend_from_slice(&[0x48, 0x8D, 0x0D]);
        code.extend_from_slice(&disp.to_le_bytes());
        let call_at = code.len();
        let b = call_at + 5 + 1 + 4 + 2; // B placed after ret and padding
        code.extend_from_slice(&[0xE8]);
        code.extend_from_slice(&((b - (call_at + 5)) as i32).to_le_bytes());
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
        code.extend_from_slice(&[0xCC, 0xCC]);
        assert_eq!(code.len(), b + 0 - 0 + (code.len() - b));
        let b = code.len();
        code.extend_from_slice(&[0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0xC3]);
        // patch the call displacement now that B's real position is known
        let d = (b - (call_at + 5)) as i32;
        code[call_at + 1..call_at + 5].copy_from_slice(&d.to_le_bytes());
        let f = module(&code);
        let m = Module::from_file_bytes(&f).unwrap();
        let site = Site::new("b", &[Strategy::CalleeOfAnchor(STR, 0)], Entry::FunctionStart, Risk::Low);
        let l = resolve(&m, &site).unwrap();
        assert_eq!(l.rva, 0x1000 + b as u32);
        let none = Site::new("b", &[Strategy::CalleeOfAnchor(STR, 3)], Entry::FunctionStart, Risk::Low);
        assert!(resolve(&m, &none).unwrap_err().to_string().contains("no callee #3"));
    }

    #[test]
    fn no_strategies_is_an_error() {
        let f = module(&sample());
        let m = Module::from_file_bytes(&f).unwrap();
        assert_eq!(
            resolve(&m, &Site::new("x", &[], Entry::Anywhere, Risk::Low)),
            Err(ResolveError::NoStrategies)
        );
    }
}
