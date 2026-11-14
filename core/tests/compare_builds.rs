//! Compares the sites of two game builds: where each moved and how much of the function's code stayed the same.
//! Needs `GW2_EXE_OLD` and `GW2_EXE_NEW` (defaults: `re/Gw2-64.exe` and no run without the second). Prints a table.

use std::path::PathBuf;

use gw2enhanced_core::{Module, Pattern};

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

/// Fraction of the first `n` bytes that are equal once rel32/disp32 operands (E8/E9 calls and RIP-relative
/// operands) are ignored: a crude "same code, moved" measure.
fn similarity(a: &[u8], b: &[u8]) -> f32 {
    let n = a.len().min(b.len());
    let mut same = 0;
    let mut i = 0;
    while i < n {
        if (a[i] == 0xE8 || a[i] == 0xE9) && a[i] == b[i] && i + 5 <= n {
            same += 5;
            i += 5;
            continue;
        }
        if a[i] == b[i] {
            same += 1;
        }
        i += 1;
    }
    same as f32 / n as f32
}

#[test]
fn compare_two_builds() {
    let (Some(old), Some(new)) = (std::env::var_os("GW2_EXE_OLD"), std::env::var_os("GW2_EXE_NEW")) else {
        eprintln!("skipped: set GW2_EXE_OLD and GW2_EXE_NEW");
        return;
    };
    let (ob, nb) = (std::fs::read(old).unwrap(), std::fs::read(new).unwrap());
    let (o, n) = (Module::from_file_bytes(&ob).unwrap(), Module::from_file_bytes(&nb).unwrap());
    let mut low = 0;
    for (name, sig) in all_sites() {
        let p = Pattern::parse(&sig).unwrap();
        let (Some(ao), Some(an)) = (p.find_unique(o.text), p.find_unique(n.text)) else {
            eprintln!("  LOST   {name}");
            continue;
        };
        let (ro, rn) = (o.text_rva + ao as u32, n.text_rva + an as u32);
        let ca = o.code_at(ro, 256).unwrap_or(&[]);
        let cb = n.code_at(rn, 256).unwrap_or(&[]);
        let s = similarity(ca, cb);
        if s < 0.9 {
            low += 1;
        }
        eprintln!("  {name:30} {ro:#09x} -> {rn:#09x}  moved {:+8}  same {:.0}%", rn as i64 - ro as i64, s * 100.0);
    }
    eprintln!("{low} sites with under 90% similarity in the first 256 bytes");
}
