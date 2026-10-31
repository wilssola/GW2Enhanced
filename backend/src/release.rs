//! Dictionary release files (pure part): records, chunking, manifest.
//!
//! Files: `releases/{lang}/{version}/dictionary-NNN.jsonl.gz` in R2, one
//! record per English text: `{"en","tr","src","ids"}` (`ids` omitted when empty).

use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::chunk::{CHUNK_RAW_BYTES, Chunk, Chunker};
use crate::hash::hex;
use crate::manifest::{self, Manifest};

pub const FILE_PREFIX: &str = "dictionary";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DictRecord {
    pub en: String,
    pub tr: String,
    /// `nemotron` | `argos` | `user`.
    pub src: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<u32>,
}

/// Stored translation source → the `src` the mod expects.
pub fn release_src(source: &str) -> &'static str {
    match source {
        "argos" => "argos",
        "user" => "user",
        _ => "nemotron",
    }
}

/// Parses the `ids` column (`"1601,1602"` from `group_concat`), sorted, unique.
pub fn parse_ids(s: Option<&str>) -> Vec<u32> {
    let mut v: Vec<u32> = s
        .unwrap_or("")
        .split(',')
        .filter_map(|x| x.trim().parse().ok())
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

pub fn r2_key(lang: &str, version: u64, name: &str) -> String {
    format!("releases/{lang}/{version}/{name}")
}

/// Accumulates records into chunks and a content hash (to skip a build when
/// nothing changed since the active release).
pub struct Builder {
    chunker: Chunker,
    hash: Sha256,
    pub count: usize,
}

impl Default for Builder {
    fn default() -> Self {
        Self::with_limit(CHUNK_RAW_BYTES)
    }
}

impl Builder {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            chunker: Chunker::new(FILE_PREFIX, limit),
            hash: Sha256::new(),
            count: 0,
        }
    }

    /// Records must come in a stable order (by English text) so equal content
    /// gives an equal hash.
    pub fn push(&mut self, r: &DictRecord) {
        self.hash.update(serde_json::to_vec(r).expect("record"));
        self.hash.update(b"\n");
        self.chunker.push(r);
        self.count += 1;
    }

    /// Returns the files and the content hash (hex).
    pub fn finish(self) -> (Vec<Chunk>, String) {
        (self.chunker.finish(), hex(&self.hash.finalize()))
    }
}

/// Signed manifest for finished files.
pub fn signed_manifest(lang: &str, version: u64, created_at: &str, files: &[Chunk], key: &SigningKey) -> Manifest {
    let m = Manifest {
        lang: lang.to_owned(),
        version,
        created_at: created_at.to_owned(),
        files: files.iter().map(|c| c.entry.clone()).collect(),
        signature: None,
    };
    manifest::sign(m, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::gunzip_limited;

    #[test]
    fn record_format() {
        let r = DictRecord {
            en: "The Cost of Victory".into(),
            tr: "O Custo da Vitória".into(),
            src: "argos".into(),
            ids: vec![1601, 1602],
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"en":"The Cost of Victory","tr":"O Custo da Vitória","src":"argos","ids":[1601,1602]}"#
        );
        let r = DictRecord { ids: vec![], ..r };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"en":"The Cost of Victory","tr":"O Custo da Vitória","src":"argos"}"#
        );
        assert_eq!(release_src("ai"), "nemotron");
        assert_eq!(parse_ids(Some("1602,1601,1601,x")), vec![1601, 1602]);
        assert_eq!(parse_ids(None), Vec::<u32>::new());
    }

    #[test]
    fn build_sign_and_verify() {
        let mut b = Builder::with_limit(80);
        for i in 0..10 {
            b.push(&DictRecord {
                en: format!("Text {i}"),
                tr: format!("Texto {i}"),
                src: "nemotron".into(),
                ids: vec![i],
            });
        }
        let (files, hash) = b.finish();
        assert!(files.len() > 1);
        assert!(files.iter().all(|f| f.entry.name.starts_with("dictionary-")));
        let first = String::from_utf8(gunzip_limited(&files[0].bytes, 1 << 20).unwrap()).unwrap();
        assert!(first.starts_with(r#"{"en":"Text 0","tr":"Texto 0","src":"nemotron","ids":[0]}"#));
        let key = SigningKey::from_bytes(&[1u8; 32]);
        let m = signed_manifest("pt-BR", 3, "2026-10-31T19:00:00Z", &files, &key);
        let body = serde_json::to_string(&m).unwrap();
        assert_eq!(manifest::verify(&body, &manifest::public_key_b64(&key)), Ok(()));
        // Same content, same hash.
        let mut b2 = Builder::with_limit(80);
        for i in 0..10 {
            b2.push(&DictRecord {
                en: format!("Text {i}"),
                tr: format!("Texto {i}"),
                src: "nemotron".into(),
                ids: vec![i],
            });
        }
        assert_eq!(b2.finish().1, hash);
        assert_eq!(
            r2_key("pt-BR", 3, "dictionary-000.jsonl.gz"),
            "releases/pt-BR/3/dictionary-000.jsonl.gz"
        );
    }
}
