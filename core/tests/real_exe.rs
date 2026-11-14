//! The engine against the real `Gw2-64.exe`: every signature the mods use today must resolve to the same single
//! address the mods' own search finds. Skipped when the exe is not available
//! (`GW2_EXE=<path>`, or `re/Gw2-64.exe` at the repo root).

use std::path::PathBuf;

use gw2enhanced_core::{Module, Pattern};

fn exe() -> Option<PathBuf> {
    let p = std::env::var_os("GW2_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../re/Gw2-64.exe"));
    p.is_file().then_some(p)
}

/// `(name, sig)` pairs written as `name: "...", sig: "..."` in a mod's source.
fn sites_in(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("name: \"") {
        rest = &rest[i + 7..];
        let Some(end) = rest.find('"') else { break };
        let name = rest[..end].to_string();
        let tail = &rest[end..];
        // `sig:` must follow within the same struct literal
        if let Some(j) = tail.find("sig: \"").filter(|&j| j < 120) {
            let s = &tail[j + 6..];
            if let Some(e) = s.find('"') {
                out.push((name, s[..e].to_string()));
            }
        }
    }
    out
}

fn sources() -> Vec<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut v = vec![root.join("translated/src/sig.rs")];
    for e in std::fs::read_dir(root.join("remastered/native/runtime/src"))
        .unwrap()
        .flatten()
    {
        v.push(e.path());
    }
    v.retain(|p| p.extension().is_some_and(|x| x == "rs"));
    v
}

#[test]
fn every_current_signature_resolves_uniquely() {
    let Some(path) = exe() else {
        eprintln!("skipped: no Gw2-64.exe (set GW2_EXE)");
        return;
    };
    let bytes = std::fs::read(path).unwrap();
    let m = Module::from_file_bytes(&bytes).expect("PE");
    let mut total = 0;
    let mut bad = Vec::new();
    for src in sources() {
        let text = std::fs::read_to_string(&src).unwrap();
        for (name, sig) in sites_in(&text) {
            total += 1;
            let p = Pattern::parse(&sig).unwrap_or_else(|| panic!("{name}: bad pattern"));
            let all = p.find_all(m.text);
            if all.len() != 1 {
                bad.push(format!("{name}: {} matches", all.len()));
            }
        }
    }
    eprintln!("{total} signatures checked, fingerprint {:016x}", m.fingerprint());
    assert!(
        total >= 39,
        "expected the mods' sites to be found in the sources, got {total}"
    );
    assert!(bad.is_empty(), "signatures that do not resolve uniquely: {bad:?}");
}
