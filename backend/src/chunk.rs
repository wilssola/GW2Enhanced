//! Release files: JSON Lines split into ~2 MB chunks, gzipped; plus a bounded
//! gunzip for request bodies.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::Serialize;

use crate::hash::sha256_hex;
use crate::manifest::FileEntry;

/// Target size of a chunk, uncompressed JSONL bytes. Game text gzips about
/// 3-4x, so the `.gz` files land well under 2 MB each, and a file never
/// exceeds this uncompressed unless a single line does.
pub const CHUNK_RAW_BYTES: usize = 2 * 1024 * 1024;

/// One finished file.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub entry: FileEntry,
    pub bytes: Vec<u8>,
}

/// Collects JSON Lines records and cuts them into gzipped files named
/// `{prefix}-NNN.jsonl.gz`.
pub struct Chunker {
    prefix: String,
    limit: usize,
    buf: Vec<u8>,
    lines: usize,
    done: Vec<Chunk>,
}

impl Chunker {
    pub fn new(prefix: &str, limit: usize) -> Self {
        Self {
            prefix: prefix.to_owned(),
            limit: limit.max(1),
            buf: Vec::new(),
            lines: 0,
            done: Vec::new(),
        }
    }

    /// Adds one record (one line). Starts a new chunk when this line would push
    /// the current one past the limit.
    pub fn push<T: Serialize>(&mut self, record: &T) {
        let mut line = serde_json::to_vec(record).expect("record");
        line.push(b'\n');
        if !self.buf.is_empty() && self.buf.len() + line.len() > self.limit {
            self.cut();
        }
        self.buf.extend_from_slice(&line);
        self.lines += 1;
    }

    fn cut(&mut self) {
        let raw = std::mem::take(&mut self.buf);
        self.lines = 0;
        let bytes = gzip(&raw);
        let name = format!("{}-{:03}.jsonl.gz", self.prefix, self.done.len());
        self.done.push(Chunk {
            entry: FileEntry {
                name,
                sha256: sha256_hex(&bytes),
                size: bytes.len() as u64,
            },
            bytes,
        });
    }

    /// Finishes; always returns at least one file (empty dictionaries still
    /// produce `-000`, so the mod always finds the same names).
    pub fn finish(mut self) -> Vec<Chunk> {
        if !self.buf.is_empty() || self.done.is_empty() {
            self.cut();
        }
        self.done
    }
}

pub fn gzip(raw: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::with_capacity(raw.len() / 3 + 64), Compression::new(6));
    enc.write_all(raw).expect("in-memory gzip");
    enc.finish().expect("in-memory gzip")
}

/// Gunzips at most `limit` bytes; `Err(TooLarge)` beyond that (zip bombs).
pub fn gunzip_limited(data: &[u8], limit: usize) -> Result<Vec<u8>, GunzipError> {
    let mut out = Vec::new();
    GzDecoder::new(data)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| GunzipError::Invalid)?;
    if out.len() > limit {
        return Err(GunzipError::TooLarge);
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
pub enum GunzipError {
    Invalid,
    TooLarge,
}

pub fn is_gzip(data: &[u8]) -> bool {
    data.starts_with(&[0x1f, 0x8b])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lines(chunk: &Chunk) -> Vec<String> {
        let raw = gunzip_limited(&chunk.bytes, usize::MAX / 2).unwrap();
        String::from_utf8(raw).unwrap().lines().map(str::to_owned).collect()
    }

    #[test]
    fn splits_at_the_limit_and_keeps_every_line() {
        let mut c = Chunker::new("strings", 100);
        for i in 0..20 {
            c.push(&json!({"en": format!("Text {i}"), "tr": format!("Texto {i}"), "src": "ai"}));
        }
        let files = c.finish();
        assert!(files.len() > 1);
        assert_eq!(files[0].entry.name, "strings-000.jsonl.gz");
        assert_eq!(files[1].entry.name, "strings-001.jsonl.gz");
        let all: Vec<String> = files.iter().flat_map(lines).collect();
        assert_eq!(all.len(), 20);
        assert_eq!(all[0], r#"{"en":"Text 0","src":"ai","tr":"Texto 0"}"#);
        for f in &files {
            let raw = gunzip_limited(&f.bytes, 1 << 20).unwrap();
            assert!(raw.len() <= 100);
            assert_eq!(f.entry.sha256, sha256_hex(&f.bytes));
            assert_eq!(f.entry.size, f.bytes.len() as u64);
        }
    }

    #[test]
    fn oversized_line_gets_its_own_chunk_and_empty_still_has_one_file() {
        let mut c = Chunker::new("ids", 10);
        c.push(&json!({"id": 1, "tr": "a very long translation"}));
        c.push(&json!({"id": 2, "tr": "b"}));
        assert_eq!(c.finish().len(), 2);
        let files = Chunker::new("ids", 10).finish();
        assert_eq!(files.len(), 1);
        assert!(lines(&files[0]).is_empty());
    }

    #[test]
    fn multiline_text_stays_on_one_line() {
        let mut c = Chunker::new("strings", CHUNK_RAW_BYTES);
        c.push(&json!({"en": "A\nB", "tr": "C\nD"}));
        assert_eq!(lines(&c.finish()[0]).len(), 1);
    }

    #[test]
    fn gunzip_is_bounded() {
        let bomb = gzip(&vec![b'a'; 1_000_000]);
        assert!(bomb.len() < 10_000);
        assert_eq!(gunzip_limited(&bomb, 256 * 1024), Err(GunzipError::TooLarge));
        assert_eq!(gunzip_limited(&bomb, 1_000_000).unwrap().len(), 1_000_000);
        assert_eq!(gunzip_limited(b"not gzip", 10), Err(GunzipError::Invalid));
        assert!(is_gzip(&bomb));
    }
}
