//! Release manifest and its Ed25519 signature.
//!
//! Signing input: the manifest as a JSON object **without** the `signature`
//! field, serialized canonically (see [`canonical_json`]): keys sorted by
//! their UTF-8 bytes at every level, no whitespace, strings escaped exactly as
//! `serde_json` does. The signature is the 64-byte Ed25519 signature of those
//! bytes, base64 (standard alphabet, padded).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    /// Lowercase hex SHA-256 of the file bytes (the `.gz` as served).
    pub sha256: String,
    pub size: u64,
}

/// `GET /v1/releases/{lang}/latest` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub lang: String,
    pub version: u64,
    pub created_at: String,
    pub files: Vec<FileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Writes `v` as canonical JSON: object keys sorted, no whitespace.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).expect("string"));
                out.push(':');
                write_value(&map[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).expect("scalar")),
    }
}

/// The exact bytes that are signed: the manifest JSON without `signature`.
pub fn signing_input(manifest_json: &Value) -> Vec<u8> {
    let mut v = manifest_json.clone();
    if let Value::Object(map) = &mut v {
        map.remove("signature");
    }
    canonical_json(&v).into_bytes()
}

/// Parses `RELEASE_SIGNING_KEY`: base64 of the 32-byte Ed25519 seed (or of the
/// 64-byte seed‖public key pair, as many tools export).
pub fn signing_key(b64: &str) -> Result<SigningKey, String> {
    let bytes = B64
        .decode(b64.trim())
        .map_err(|e| format!("RELEASE_SIGNING_KEY is not base64: {e}"))?;
    let seed: [u8; 32] = match bytes.len() {
        32 | 64 => bytes[..32].try_into().unwrap(),
        n => return Err(format!("RELEASE_SIGNING_KEY must be 32 or 64 bytes, got {n}")),
    };
    let key = SigningKey::from_bytes(&seed);
    if bytes.len() == 64 && key.verifying_key().as_bytes() != &bytes[32..] {
        return Err("RELEASE_SIGNING_KEY: public half does not match the seed".into());
    }
    Ok(key)
}

/// Public key as the mod ships it: base64 of the raw 32 bytes.
pub fn public_key_b64(key: &SigningKey) -> String {
    B64.encode(key.verifying_key().as_bytes())
}

/// Signs the manifest; returns it with `signature` set.
pub fn sign(mut m: Manifest, key: &SigningKey) -> Manifest {
    m.signature = None;
    let v = serde_json::to_value(&m).expect("manifest");
    let sig = key.sign(&signing_input(&v));
    m.signature = Some(B64.encode(sig.to_bytes()));
    m
}

/// Verifies a manifest body (as received) against a base64 public key, the way
/// the mod must do it.
pub fn verify(body: &str, public_key_b64: &str) -> Result<(), String> {
    let v: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let sig = v.get("signature").and_then(Value::as_str).ok_or("no signature")?;
    let sig = B64.decode(sig).map_err(|e| e.to_string())?;
    let sig = Signature::from_slice(&sig).map_err(|e| e.to_string())?;
    let pk: [u8; 32] = B64
        .decode(public_key_b64.trim())
        .map_err(|e| e.to_string())?
        .try_into()
        .map_err(|_| "public key must be 32 bytes")?;
    let pk = VerifyingKey::from_bytes(&pk).map_err(|e| e.to_string())?;
    pk.verify(&signing_input(&v), &sig).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn manifest() -> Manifest {
        Manifest {
            lang: "pt-BR".into(),
            version: 42,
            created_at: "2026-10-31T19:00:00Z".into(),
            files: vec![
                FileEntry {
                    name: "strings-000.jsonl.gz".into(),
                    sha256: "ab".repeat(32),
                    size: 123456,
                },
                FileEntry {
                    name: "ids-000.jsonl.gz".into(),
                    sha256: "cd".repeat(32),
                    size: 99,
                },
            ],
            signature: None,
        }
    }

    #[test]
    fn canonical_form_is_sorted_and_compact() {
        let v: Value = serde_json::from_str(r#"{ "b": 1, "a": {"y": "é\n", "x": [true, null]} }"#).unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":{"x":[true,null],"y":"é\n"},"b":1}"#);
    }

    #[test]
    fn exact_signing_input() {
        let v = serde_json::to_value(sign(manifest(), &key())).unwrap();
        let input = String::from_utf8(signing_input(&v)).unwrap();
        let expected = format!(
            r#"{{"created_at":"2026-10-31T19:00:00Z","files":[{{"name":"strings-000.jsonl.gz","sha256":"{}","size":123456}},{{"name":"ids-000.jsonl.gz","sha256":"{}","size":99}}],"lang":"pt-BR","version":42}}"#,
            "ab".repeat(32),
            "cd".repeat(32)
        );
        assert_eq!(input, expected);
        // With serde_json's default (sorted) map, a plain re-serialization is
        // the same: that is all the mod needs to do.
        let mut plain = v.clone();
        plain.as_object_mut().unwrap().remove("signature");
        assert_eq!(serde_json::to_string(&plain).unwrap(), expected);
    }

    #[test]
    fn sign_then_verify_and_detect_tampering() {
        let k = key();
        let pk = public_key_b64(&k);
        let body = serde_json::to_string_pretty(&sign(manifest(), &k)).unwrap();
        assert_eq!(verify(&body, &pk), Ok(()));
        let tampered = body.replace("123456", "123457");
        assert!(verify(&tampered, &pk).is_err());
        let other = public_key_b64(&SigningKey::from_bytes(&[8u8; 32]));
        assert!(verify(&body, &other).is_err());
    }

    #[test]
    fn parses_32_and_64_byte_keys() {
        let k = key();
        let seed = B64.encode(k.to_bytes());
        let pair = B64.encode(k.to_keypair_bytes());
        assert_eq!(signing_key(&seed).unwrap().to_bytes(), k.to_bytes());
        assert_eq!(signing_key(&pair).unwrap().to_bytes(), k.to_bytes());
        assert!(signing_key("AAAA").is_err());
        let mut bad = k.to_keypair_bytes();
        bad[63] ^= 1;
        assert!(signing_key(&B64.encode(bad)).is_err());
    }
}
