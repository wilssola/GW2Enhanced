//! How many of the mods' real sites could also be found through a string the function references?
//!
//! This is the evidence behind the `StringXref` strategy: a site with such an anchor survives a recompilation that
//! changes its bytes. Prints the coverage; asserts only that the engine finds anchors at all. Skipped without the exe.

use std::path::PathBuf;

use gw2enhanced_core::{Entry, Module, Pattern, Risk, Site, Strategy, resolve};

fn exe() -> Option<PathBuf> {
    let p = std::env::var_os("GW2_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../re/Gw2-64.exe"));
    p.is_file().then_some(p)
}

fn sites_in(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("name: \"") {
        rest = &rest[i + 7..];
        let Some(end) = rest.find('"') else { break };
        let name = rest[..end].to_string();
        let tail = &rest[end..];
        if let Some(j) = tail.find("sig: \"").filter(|&j| j < 120) {
            let s = &tail[j + 6..];
            if let Some(e) = s.find('"') {
                out.push((name, s[..e].to_string()));
            }
        }
    }
    out
}

fn all_sites() -> Vec<(String, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = vec![root.join("translated/src/sig.rs")];
    for e in std::fs::read_dir(root.join("remastered/native/runtime/src")).unwrap().flatten() {
        files.push(e.path());
    }
    files.retain(|p| p.extension().is_some_and(|x| x == "rs"));
    files.iter().flat_map(|p| sites_in(&std::fs::read_to_string(p).unwrap())).collect()
}

/// Printable strings (8..80 chars) that the code in `[rva, rva+len)` loads with `lea reg,[rip+disp32]`.
fn strings_used(m: &Module, rva: u32, len: usize) -> Vec<String> {
    let code = m.code_at(rva, len).unwrap();
    let mut out = Vec::new();
    for i in 0..code.len().saturating_sub(7) {
        if matches!(code[i], 0x48 | 0x4C | 0x49 | 0x4D) && code[i + 1] == 0x8D && code[i + 2] & 0xC7 == 0x05 {
            let disp = i32::from_le_bytes(code[i + 3..i + 7].try_into().unwrap());
            let target = (rva as i64 + i as i64 + 7 + disp as i64) as u32;
            for (drva, bytes) in &m.data {
                if target >= *drva && ((target - drva) as usize) < bytes.len() {
                    let b = &bytes[(target - drva) as usize..];
                    let n = b.iter().position(|&c| c == 0).unwrap_or(0);
                    if (8..80).contains(&n) && b[..n].iter().all(|c| (0x20..0x7F).contains(c)) {
                        let s = String::from_utf8_lossy(&b[..n]).to_string();
                        if !out.contains(&s) {
                            out.push(s);
                        }
                    }
                }
            }
        }
    }
    out
}

#[test]
fn how_many_sites_have_a_string_anchor() {
    let Some(path) = exe() else {
        eprintln!("skipped: no Gw2-64.exe");
        return;
    };
    let bytes = std::fs::read(path).unwrap();
    let m = Module::from_file_bytes(&bytes).unwrap();
    let (mut with, mut total) = (0, 0);
    let mut without = Vec::new();
    for (name, sig) in all_sites() {
        total += 1;
        let rva = m.text_rva + Pattern::parse(&sig).unwrap().find_unique(m.text).unwrap() as u32;
        let mut hit = None;
        for s in strings_used(&m, rva, 0x600).into_iter().take(6) {
            let s: &'static str = Box::leak(s.into_boxed_str());
            let strategies: &'static [Strategy] = Box::leak(Box::new([Strategy::StringXref(s)]));
            let site = Site::new("probe", strategies, Entry::FunctionStart, Risk::Low);
            if resolve(&m, &site).is_ok_and(|l| l.rva == rva) {
                hit = Some(s);
                break;
            }
        }
        match hit {
            Some(s) => {
                with += 1;
                eprintln!("  anchor  {name:28} {s:?}");
            }
            None => without.push(name),
        }
    }
    eprintln!("string anchor for {with} of {total} sites; none for: {without:?}");
    assert!(with > 0);
}

/// Second anchor kind: the site is called (`call rel32`) by a function that a unique string identifies.
#[test]
fn how_many_sites_are_called_from_an_anchored_function() {
    use std::collections::{HashMap, HashSet};
    let Some(path) = exe() else { return };
    let bytes = std::fs::read(path).unwrap();
    let m = Module::from_file_bytes(&bytes).unwrap();
    let t = m.text;
    let start_of = |at: usize| -> Option<u32> {
        let lo = at.saturating_sub(0x4000);
        (lo + 1..=at).rev().find(|&p| t[p - 1] == 0xCC && t[p] != 0xCC).map(|p| m.text_rva + p as u32)
    };
    // one pass: string target -> functions that load it, and call target -> call sites
    let in_data = |r: u32| m.data.iter().any(|(d, b)| r >= *d && ((r - d) as usize) < b.len());
    let mut str_funcs: HashMap<u32, HashSet<u32>> = HashMap::new();
    let mut callers: HashMap<u32, Vec<usize>> = HashMap::new();
    for i in 0..t.len().saturating_sub(7) {
        if matches!(t[i], 0x48 | 0x4C | 0x49 | 0x4D) && t[i + 1] == 0x8D && t[i + 2] & 0xC7 == 0x05 {
            let disp = i32::from_le_bytes(t[i + 3..i + 7].try_into().unwrap());
            let target = (m.text_rva as i64 + i as i64 + 7 + disp as i64) as u32;
            if in_data(target) {
                if let Some(f) = start_of(i) {
                    str_funcs.entry(target).or_default().insert(f);
                }
            }
        }
        if t[i] == 0xE8 {
            let disp = i32::from_le_bytes(t[i + 1..i + 5].try_into().unwrap());
            let target = (m.text_rva as i64 + i as i64 + 5 + disp as i64) as u32;
            if m.contains_code(target) {
                callers.entry(target).or_default().push(i);
            }
        }
    }
    // anchored functions: those that load a string no other function loads
    let anchored: HashSet<u32> =
        str_funcs.values().filter(|fs| fs.len() == 1).flat_map(|fs| fs.iter().copied()).collect();
    eprintln!("{} anchored functions, {} call targets", anchored.len(), callers.len());
    let (mut direct, mut via_call, mut neither) = (0, 0, Vec::new());
    for (name, sig) in all_sites() {
        let rva = m.text_rva + Pattern::parse(&sig).unwrap().find_unique(t).unwrap() as u32;
        if anchored.contains(&rva) {
            direct += 1;
            continue;
        }
        let cs = callers.get(&rva).map(|v| v.as_slice()).unwrap_or(&[]);
        let anchored_callers: HashSet<u32> =
            cs.iter().filter_map(|&c| start_of(c)).filter(|f| anchored.contains(f)).collect();
        if !anchored_callers.is_empty() {
            via_call += 1;
        } else {
            neither.push(format!("{name}({} callers)", cs.len()));
        }
    }
    eprintln!("direct anchor {direct}, called from an anchored function {via_call}, neither {}: {neither:?}", neither.len());
}
