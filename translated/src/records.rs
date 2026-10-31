//! Data files in JSON Lines: one JSON object per line.
//!
//! | File | Record |
//! |---|---|
//! | `languages\<language>\dictionary.jsonl` | [`Translation`] (shipped, reviewed dictionary) |
//! | `languages\<language>\local.jsonl` | [`Translation`] (made on the player's PC) |
//! | `discovered.jsonl` | [`Discovered`] (texts seen without a translation) |
//! | `decrypted.jsonl` | [`Decrypted`] (protected strings already decrypted) |
//!
//! Why JSON Lines: a record is always one line (game texts often contain line
//! breaks, which an escaped `\n` keeps inside the string), so files append
//! safely, diff cleanly in git and can grow new fields. A line cut short by a
//! crash is skipped when reading instead of failing the whole file.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// English source text, its translation and the game's text IDs for it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Translation {
    pub en: String,
    pub tr: String,
    /// Where it came from (`nemotron`, `argos`, a reviewer...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src: Option<String>,
    /// Text IDs with this English text (the same in every client language, so
    /// they also translate a game running in another language).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<u32>,
}

/// English text seen in the game without a translation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discovered {
    pub en: String,
}

/// Protected (RC4) string, decrypted with the key the server sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decrypted {
    pub id: u32,
    pub en: String,
}

/// Records read from a file, plus how many lines could not be parsed.
pub struct Read<T> {
    pub records: Vec<T>,
    pub skipped: usize,
}

/// Reads every record; a missing file reads as empty.
pub fn read<T: DeserializeOwned>(path: &Path) -> io::Result<Read<T>> {
    let mut out = Read {
        records: Vec::new(),
        skipped: 0,
    };
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    read_from(BufReader::new(file), &mut out)?;
    Ok(out)
}

fn read_from<T: DeserializeOwned, R: BufRead>(reader: R, out: &mut Read<T>) -> io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        let line = line.trim_start_matches('\u{feff}');
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(r) => out.records.push(r),
            Err(_) => out.skipped += 1,
        }
    }
    Ok(())
}

/// Parses records from text (tests and tools).
pub fn parse<T: DeserializeOwned>(text: &str) -> Read<T> {
    let mut out = Read {
        records: Vec::new(),
        skipped: 0,
    };
    let _ = read_from(text.as_bytes(), &mut out);
    out
}

/// Appends records to a file, creating it if needed.
pub struct Appender {
    out: BufWriter<File>,
}

impl Appender {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            out: BufWriter::new(file),
        })
    }

    pub fn write<T: Serialize>(&mut self, record: &T) -> io::Result<()> {
        serde_json::to_writer(&mut self.out, record).map_err(io::Error::other)?;
        self.out.write_all(b"\n")
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Rewrites a whole file; goes through a temporary file, so a crash never
/// leaves it half written.
pub fn write_all<'a, T: Serialize + 'a>(path: &Path, records: impl IntoIterator<Item = &'a T>) -> io::Result<()> {
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut out = BufWriter::new(File::create(&tmp)?);
        for r in records {
            serde_json::to_writer(&mut out, r).map_err(io::Error::other)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_keeps_line_breaks_inside_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let a = Translation {
            en: "Line1\nLine2 \"q\"".into(),
            tr: "Linha1\nLinha2".into(),
            ..Default::default()
        };
        let b = Translation {
            en: "Hi".into(),
            tr: "Oi".into(),
            src: Some("argos".into()),
            ids: vec![1, 2],
        };
        let mut w = Appender::open(&p).unwrap();
        w.write(&a).unwrap();
        w.write(&b).unwrap();
        w.flush().unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(!text.contains("\"src\":null") && !text.contains("\"ids\":[]"));
        let r: Read<Translation> = read(&p).unwrap();
        assert_eq!(r.records, vec![a, b]);
    }

    #[test]
    fn skips_a_line_cut_short_and_reads_missing_file_as_empty() {
        let r: Read<Decrypted> = parse("{\"id\":1,\"en\":\"a\"}\n{\"id\":2,\"en\":\"b");
        assert_eq!((r.records.len(), r.skipped), (1, 1));
        let r: Read<Discovered> = read(Path::new("does-not-exist.jsonl")).unwrap();
        assert!(r.records.is_empty());
    }

    #[test]
    fn write_all_replaces_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("decrypted.jsonl");
        std::fs::write(&p, "old\n").unwrap();
        write_all(&p, &[Decrypted { id: 7, en: "x".into() }]).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"id\":7,\"en\":\"x\"}\n");
    }
}
