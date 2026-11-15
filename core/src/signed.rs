//! Signed release checksums.
//!
//! A release carries `SHA256SUMS.txt` (the SHA-256 of every zip) and `SHA256SUMS.txt.sig`: the base64 of an ed25519
//! signature over the exact bytes of `SHA256SUMS.txt`. The private key exists only as the CI secret
//! `UPDATE_SIGNING_KEY` (and the player's offline copy); the public key is built into the DLL. So a zip swapped on the
//! release page, or a checksum file edited next to it, is refused: the checksums alone only catch a broken download.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

/// The public half of the CI secret `UPDATE_SIGNING_KEY` (base64 of the raw 32 bytes). Rotating the key means a new
/// DLL; releases signed with the old key are then refused by it.
pub const UPDATE_PUBLIC_KEY: &str = "qxXQH+8xIJU6fzYhVm/oqgiSJV9TXqiLdOQN0uOxiGA=";

/// Checks the signature of a release's checksums against the built-in [`UPDATE_PUBLIC_KEY`].
pub fn verify(sums: &[u8], sig_b64: &str) -> Result<(), String> {
    verify_with(sums, sig_b64, UPDATE_PUBLIC_KEY)
}

/// Checks `sig_b64` (the content of `SHA256SUMS.txt.sig`) over `sums` with the public key `public_b64` (base64 of the
/// raw 32 bytes).
pub fn verify_with(sums: &[u8], sig_b64: &str, public_b64: &str) -> Result<(), String> {
    let key: [u8; 32] = B64
        .decode(public_b64.trim())
        .ok()
        .and_then(|k| k.try_into().ok())
        .ok_or("bad built-in public key")?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| "bad built-in public key")?;
    let sig: [u8; 64] = B64
        .decode(sig_b64.trim())
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or("the checksum signature is malformed")?;
    key.verify_strict(sums, &Signature::from_bytes(&sig))
        .map_err(|_| "the checksums are not signed by GW2 Enhanced".to_string())
}

/// The signature file content (base64) for `sums`, with the private key `seed_b64` (base64 of the 32-byte seed).
pub fn sign(sums: &[u8], seed_b64: &str) -> Result<String, String> {
    let seed: [u8; 32] = B64
        .decode(seed_b64.trim())
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or("the signing key must be the base64 of a 32-byte seed")?;
    Ok(B64.encode(SigningKey::from_bytes(&seed).sign(sums).to_bytes()))
}

/// The public key (base64) of a private seed.
pub fn public_of(seed_b64: &str) -> Result<String, String> {
    let seed: [u8; 32] = B64
        .decode(seed_b64.trim())
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or("the signing key must be the base64 of a 32-byte seed")?;
    Ok(B64.encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="; // 1..=32

    #[test]
    fn a_signature_verifies_only_for_the_same_bytes_and_key() {
        let public = public_of(SEED).unwrap();
        let sums = b"abc  GW2Enhanced-All-1.0.0.zip\n";
        let sig = sign(sums, SEED).unwrap();
        assert!(verify_with(sums, &sig, &public).is_ok());
        assert!(verify_with(b"abd  GW2Enhanced-All-1.0.0.zip\n", &sig, &public).is_err());
        let other = public_of("ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8=").unwrap();
        assert!(verify_with(sums, &sig, &other).is_err());
        assert!(verify_with(sums, "not base64!", &public).is_err());
        assert!(verify_with(sums, &format!("{sig}\n"), &public).is_ok(), "a trailing newline in the .sig file is fine");
    }
}
