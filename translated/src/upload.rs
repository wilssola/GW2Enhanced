//! Sends collected texts to the project's server, only while the player
//! agreed to contribute (`contribute=true`). API: `backend/docs/api.md`.
//!
//! What goes: `decrypted.jsonl` (text ID + English), `discovered.jsonl`
//! (English) and `local.jsonl` (translations made on this PC). Player chat is
//! never read, saved or sent.
//!
//! The files are append-only: `upload_state.json` keeps how far each one was
//! sent, so nothing is sent twice and the backlog drains a little per cycle.
//! The position only moves after our server confirmed the batch, so whatever
//! was collected while the server was down or unreachable is sent later.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::config::Contribute;
use crate::{log, net, runtime};

const HOST: &str = "app.gw2enhanced.workers.dev";
const PATH: &str = "/v1/contributions";
/// Limits of one request (the server refuses more).
const MAX_ITEMS: usize = 500;
const MAX_BYTES: usize = 240 * 1024;
/// Requests per cycle, so a large backlog doesn't flood the server.
const MAX_REQUESTS: usize = 10;
/// Bytes read from a file per request at most.
const READ_CHUNK: u64 = 1024 * 1024;
const FIRST_DELAY: Duration = Duration::from_secs(120);
const INTERVAL: Duration = Duration::from_secs(300);
/// After a server error or rate limit.
const BACKOFF: Duration = Duration::from_secs(1800);

#[derive(Serialize)]
struct Item {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u32>,
    en: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    src: Option<String>,
}

#[derive(Serialize)]
struct Request<'a> {
    client: String,
    lang: &'a str,
    items: &'a [Item],
}

pub struct Setup {
    pub state_file: PathBuf,
    /// Files to send, each with the language its translations are in (if any).
    pub files: Vec<PathBuf>,
    pub language: String,
}

/// Our server's answer to a contribution: `{"accepted": n, ...}`.
fn accepted(response: &[u8]) -> bool {
    serde_json::from_slice::<Value>(response).is_ok_and(|v| v.get("accepted").is_some_and(Value::is_u64))
}

/// Our server's error answer: `{"error": "..."}`.
fn is_api_error(response: &[u8]) -> bool {
    serde_json::from_slice::<Value>(response).is_ok_and(|v| v.get("error").is_some_and(Value::is_string))
}

/// Turns a record line of any of the three files into an item.
fn item(line: &str) -> Option<Item> {
    let v: Value = serde_json::from_str(line).ok()?;
    let en = v.get("en")?.as_str()?.to_owned();
    if en.trim().is_empty() || en.trim() == crate::dict::NULL_MARKER {
        return None;
    }
    let id = v.get("id").and_then(Value::as_u64).and_then(|i| u32::try_from(i).ok());
    let tr = v.get("tr").and_then(Value::as_str).map(str::to_owned);
    let src = tr.as_ref().and(v.get("src").and_then(Value::as_str).map(str::to_owned));
    Some(Item { id, en, tr, src })
}

/// Complete lines from `offset` (at most [`READ_CHUNK`] bytes), and the
/// offset right after the last one.
fn read_lines(path: &Path, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    f.take(READ_CHUNK).read_to_end(&mut buf)?;
    let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok((Vec::new(), offset));
    };
    let text = String::from_utf8_lossy(&buf[..=end]);
    Ok((text.lines().map(str::to_owned).collect(), offset + end as u64 + 1))
}

/// Splits `lines` into requests; each batch carries the byte length of its lines.
fn batches(lines: &[String]) -> Vec<(Vec<Item>, u64)> {
    let mut out = Vec::new();
    let (mut items, mut bytes, mut consumed) = (Vec::new(), 0usize, 0u64);
    for line in lines {
        let size = line.len() + 16;
        if !items.is_empty() && (items.len() >= MAX_ITEMS || bytes + size > MAX_BYTES) {
            out.push((std::mem::take(&mut items), consumed));
            (bytes, consumed) = (0, 0);
        }
        consumed += line.len() as u64 + 1;
        if let Some(it) = item(line) {
            bytes += size;
            items.push(it);
        }
    }
    if consumed > 0 {
        out.push((items, consumed));
    }
    out
}

fn load_state(path: &Path) -> HashMap<String, u64> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, state: &HashMap<String, u64>) {
    let tmp = path.with_extension("tmp");
    if let Ok(json) = serde_json::to_string_pretty(state)
        && std::fs::write(&tmp, json).is_ok()
    {
        let _ = std::fs::rename(&tmp, path);
    }
}

enum Outcome {
    /// Everything sent so far; wait for the next cycle.
    Idle,
    /// The server is unreachable or asked us to slow down.
    Backoff,
}

/// One cycle: sends up to [`MAX_REQUESTS`] requests.
fn cycle(setup: &Setup, state: &mut HashMap<String, u64>, sent: &mut usize) -> Outcome {
    let client = format!("gw2translated/{}", crate::VERSION);
    let mut requests = 0;
    for file in &setup.files {
        let key = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        loop {
            if requests >= MAX_REQUESTS || runtime::contribute() != Contribute::Yes {
                return Outcome::Idle;
            }
            let offset = state.get(&key).copied().unwrap_or(0);
            let Ok((lines, _)) = read_lines(file, offset) else {
                break;
            };
            if lines.is_empty() {
                break;
            }
            let mut offset = offset;
            for (items, consumed) in batches(&lines) {
                if requests >= MAX_REQUESTS {
                    return Outcome::Idle;
                }
                if !items.is_empty() {
                    let body = serde_json::to_vec(&Request {
                        client: client.clone(),
                        lang: &setup.language,
                        items: &items,
                    });
                    let Ok(body) = body else { return Outcome::Backoff };
                    requests += 1;
                    match net::post_json(HOST, PATH, &body) {
                        // Only our server's answer counts: a network that answers every
                        // request itself (hotel Wi-Fi, proxy) must not mark data as sent.
                        Ok((200..=299, response)) if accepted(&response) => *sent += items.len(),
                        Ok((status @ 200..=299, _)) => {
                            log!("upload: unexpected answer (HTTP {status}); trying again later");
                            return Outcome::Backoff;
                        }
                        // Refused content won't get better by retrying: skip it.
                        Ok((400 | 413, response)) if is_api_error(&response) => {
                            log!("upload: {} items refused by the server ({key})", items.len())
                        }
                        Ok((status, _)) => {
                            log!("upload: server answered {status}; trying again later");
                            return Outcome::Backoff;
                        }
                        Err(e) => {
                            log!("upload: server unreachable ({e}); trying again later");
                            return Outcome::Backoff;
                        }
                    }
                }
                offset += consumed;
                state.insert(key.clone(), offset);
                save_state(&setup.state_file, state);
            }
        }
    }
    Outcome::Idle
}

fn run(setup: Setup) {
    let mut state = load_state(&setup.state_file);
    let mut sent = 0usize;
    std::thread::sleep(FIRST_DELAY);
    loop {
        let wait = if runtime::contribute() == Contribute::Yes {
            let before = sent;
            let outcome = cycle(&setup, &mut state, &mut sent);
            if sent > before {
                log!("upload: {} items sent this session", sent);
            }
            match outcome {
                Outcome::Idle => INTERVAL,
                Outcome::Backoff => BACKOFF,
            }
        } else {
            INTERVAL
        };
        std::thread::sleep(wait);
    }
}

pub fn start(setup: Setup) {
    let _ = std::thread::Builder::new()
        .name("gw2translated-upload".into())
        .spawn(move || run(setup));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_servers_answers_count() {
        assert!(accepted(br#"{"accepted":3,"rejected":0}"#));
        assert!(!accepted(b"<html>Hotel Wi-Fi login</html>"));
        assert!(!accepted(br#"{"ok":true}"#));
        assert!(is_api_error(br#"{"error":"too_many_items"}"#));
        assert!(!is_api_error(b"Bad Request"));
    }

    #[test]
    fn items_from_each_file_kind() {
        let d = item(r#"{"id":1601,"en":"The Cost of Victory"}"#).unwrap();
        assert_eq!((d.id, d.tr.as_deref()), (Some(1601), None));
        let l = item(r#"{"en":"Sand Lioness","tr":"Leoa de Areia","src":"argos"}"#).unwrap();
        assert_eq!(
            (l.tr.as_deref(), l.src.as_deref()),
            (Some("Leoa de Areia"), Some("argos"))
        );
        assert!(item(r#"{"en":"[null]"}"#).is_none());
        assert!(item("not json").is_none());
    }

    #[test]
    fn batches_respect_limits_and_count_every_byte() {
        let lines: Vec<String> = (0..1200).map(|i| format!(r#"{{"en":"text {i}"}}"#)).collect();
        let b = batches(&lines);
        assert_eq!(b.iter().map(|(i, _)| i.len()).collect::<Vec<_>>(), [500, 500, 200]);
        let total: u64 = lines.iter().map(|l| l.len() as u64 + 1).sum();
        assert_eq!(b.iter().map(|(_, c)| c).sum::<u64>(), total);
    }

    #[test]
    fn reads_only_complete_lines_from_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        std::fs::write(&p, "{\"en\":\"a\"}\n{\"en\":\"b\"}\n{\"en\":\"c").unwrap();
        let (lines, next) = read_lines(&p, 0).unwrap();
        assert_eq!(lines.len(), 2);
        let (lines, next2) = read_lines(&p, next).unwrap();
        assert!(lines.is_empty());
        assert_eq!(next2, next);
    }
}
