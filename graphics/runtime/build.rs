//! Exports the wtsapi32.dll forwarders under their exact undecorated names (MSVC `/EXPORT`).
//! The names come from the `with_exports!` list in `src/proxy.rs` (one `index name` pair per line).

fn main() {
    // Only the standalone wtsapi32 build exports the forwarders; as part of the unified mod the stubs are not compiled.
    if std::env::var_os("CARGO_FEATURE_STANDALONE").is_none() {
        return;
    }
    println!("cargo:rerun-if-changed=src/proxy.rs");
    let src = std::fs::read_to_string("src/proxy.rs").expect("read src/proxy.rs");
    let body = src.split("macro_rules! with_exports").nth(1).expect("with_exports!");
    let body = body.split("macro_rules! names_array").next().unwrap();
    let mut n = 0;
    for line in body.lines() {
        let mut it = line.split_whitespace();
        if let (Some(i), Some(name), None) = (it.next(), it.next(), it.next()) {
            if i.parse::<u32>().is_ok() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                println!("cargo:rustc-link-arg-cdylib=/EXPORT:{name}");
                n += 1;
            }
        }
    }
    assert_eq!(n, 78, "expected 78 exports in src/proxy.rs");
}
