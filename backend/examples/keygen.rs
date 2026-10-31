//! Generates the release signing key pair.
//!
//! `cargo run --example keygen` prints the private key (set it with
//! `wrangler secret put RELEASE_SIGNING_KEY`) and the public key the mod ships.
//! `cargo run --example keygen -- <private base64>` prints the public key of
//! an existing private key.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::SigningKey;
use gw2translated_backend::manifest;

fn main() {
    let key = match std::env::args().nth(1) {
        Some(b64) => manifest::signing_key(&b64).unwrap_or_else(|e| panic!("{e}")),
        None => {
            let mut seed = [0u8; 32];
            getrandom::getrandom(&mut seed).expect("OS randomness");
            let key = SigningKey::from_bytes(&seed);
            println!(
                "RELEASE_SIGNING_KEY (secret, base64 of the 32-byte seed):\n{}\n",
                B64.encode(key.to_bytes())
            );
            key
        }
    };
    println!(
        "Public key (ship in the mod, base64 of the raw 32 bytes):\n{}",
        manifest::public_key_b64(&key)
    );
}
