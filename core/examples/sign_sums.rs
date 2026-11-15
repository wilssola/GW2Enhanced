//! Signs a release's `SHA256SUMS.txt` for the in-game updater.
//!
//! `UPDATE_SIGNING_KEY=<base64 seed> cargo run -p gw2enhanced-core --example sign_sums -- <SHA256SUMS.txt>`
//!   writes `<SHA256SUMS.txt>.sig` (base64 ed25519 signature) and checks it against the built-in public key.
//! `cargo run -p gw2enhanced-core --example sign_sums -- --keygen`
//!   prints a new key pair: the seed goes to the CI secret `UPDATE_SIGNING_KEY` (and an offline backup), the public key
//!   into `UPDATE_PUBLIC_KEY` in `core/src/signed.rs`.

use base64::Engine as _;
use gw2enhanced_core::signed;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: sign_sums <SHA256SUMS.txt> | --keygen");
        std::process::exit(2);
    });
    if arg == "--keygen" {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("OS randomness");
        let seed = base64::engine::general_purpose::STANDARD.encode(seed);
        println!("UPDATE_SIGNING_KEY (secret): {seed}");
        println!("UPDATE_PUBLIC_KEY: {}", signed::public_of(&seed).unwrap());
        return;
    }
    let key = std::env::var("UPDATE_SIGNING_KEY").unwrap_or_default();
    if key.trim().is_empty() {
        eprintln!("UPDATE_SIGNING_KEY is not set: refusing to publish unsigned checksums");
        std::process::exit(1);
    }
    let sums = std::fs::read(&arg).unwrap_or_else(|e| panic!("read {arg}: {e}"));
    let sig = signed::sign(&sums, &key).unwrap_or_else(|e| panic!("{e}"));
    let public = signed::public_of(&key).unwrap();
    if public != signed::UPDATE_PUBLIC_KEY {
        eprintln!("UPDATE_SIGNING_KEY does not match the public key the DLL ships ({})", signed::UPDATE_PUBLIC_KEY);
        std::process::exit(1);
    }
    signed::verify(&sums, &sig).expect("signature just made must verify");
    let out = format!("{arg}.sig");
    std::fs::write(&out, format!("{sig}\n")).unwrap_or_else(|e| panic!("write {out}: {e}"));
    println!("signed {arg} -> {out} (public key {public})");
}
