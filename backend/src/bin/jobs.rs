//! The backend's daily jobs, run by GitHub Actions (`.github/workflows/backend-jobs.yml`) instead of the Worker:
//! building a dictionary release gzips and hashes the whole dictionary and a backup exports every table, which does not
//! fit the CPU time a Worker gets on the Free plan. Both talk to the same D1 database and R2 bucket through the
//! Cloudflare API and use the same statements (`crate::queries`) as the Worker.
//!
//! ```text
//! jobs release [--force]   build every language in LANGS (skips unchanged and pinned ones)
//! jobs backup              export every table to backups/<timestamp>/
//! jobs seed <dictionary.jsonl> [lang]   load the shipped dictionary into D1, a daily slice at a time
//! ```
//! The seed stays under the D1 Free plan's 100k rows written per day (`SEED_BUDGET`, default 75k, leaving room for the
//! site's own writes): every index of a table counts as one more written row. Progress is kept in `kv`
//! (`seed:{lang}` = next line, or `done`), so each run continues where the last stopped; while it is not `done`, the
//! release build of that language waits (the mod already ships the full dictionary).
//! Env: `CLOUDFLARE_API_TOKEN` (D1 edit + R2 edit), `CLOUDFLARE_ACCOUNT_ID`, `RELEASE_SIGNING_KEY` (release only);
//! optional `D1_DATABASE_ID`, `R2_BUCKET`, `LANGS` (defaults: the values in wrangler.toml).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gw2translated_backend::chunk::gzip;
use gw2translated_backend::queries::{self as q, BackupTable, Q};
use gw2translated_backend::release::{self, Builder, DictRecord, parse_ids, release_src};
use gw2translated_backend::{hash, manifest, time, validate};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const DEFAULT_DB: &str = "a3eb4cd3-c55f-4521-a5b7-4a6326740b13";
const DEFAULT_BUCKET: &str = "gw2enhanced";
const DEFAULT_LANGS: &str = "pt-BR";
const API: &str = "https://api.cloudflare.com/client/v4";

const RELEASE_PAGE: u64 = 5000;
const BACKUP_PAGE: u64 = 5000;
const BACKUP_PART_ROWS: usize = 50_000;

type Res<T> = Result<T, String>;

fn env(name: &str, default: Option<&str>) -> Res<String> {
    match std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        Some(v) => Ok(v),
        None => default.map(str::to_string).ok_or(format!("{name} is not set")),
    }
}

/// The D1 database and R2 bucket through the Cloudflare REST API.
struct Cf {
    account: String,
    db: String,
    bucket: String,
    token: String,
    agent: ureq::Agent,
}

/// A bound value as the D1 API takes it.
fn param(v: sea_query::Value) -> Value {
    use sea_query::Value as V;
    match v {
        V::Bool(x) => x.map(|b| json!(b as u8)).unwrap_or(Value::Null),
        V::TinyInt(x) => json!(x),
        V::SmallInt(x) => json!(x),
        V::Int(x) => json!(x),
        V::BigInt(x) => json!(x),
        V::TinyUnsigned(x) => json!(x),
        V::SmallUnsigned(x) => json!(x),
        V::Unsigned(x) => json!(x),
        V::BigUnsigned(x) => json!(x),
        V::Float(x) => json!(x),
        V::Double(x) => json!(x),
        V::String(x) => json!(x),
        V::Char(x) => json!(x.map(|c| c.to_string())),
        // Not produced by `crate::queries`.
        other => panic!("unsupported bound value {other:?}"),
    }
}

fn statement((sql, values): Q) -> Value {
    json!({ "sql": sql, "params": values.0.into_iter().map(param).collect::<Vec<_>>() })
}

/// `"releases/pt-BR/3/manifest.json"` with every byte but unreserved ones and `/` percent-encoded.
fn object_path(key: &str) -> String {
    key.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Cf {
    fn from_env() -> Res<Self> {
        Ok(Self {
            account: env("CLOUDFLARE_ACCOUNT_ID", None)?,
            db: env("D1_DATABASE_ID", Some(DEFAULT_DB))?,
            bucket: env("R2_BUCKET", Some(DEFAULT_BUCKET))?,
            token: env("CLOUDFLARE_API_TOKEN", None)?,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(120)))
                // Some networks resolve IPv6 but cannot route it; the API answers on IPv4 everywhere.
                .ip_family(ureq::config::IpFamily::Ipv4Only)
                .http_status_as_error(false)
                .build()
                .into(),
        })
    }

    fn d1(&self, body: &Value) -> Res<Vec<Value>> {
        let url = format!("{API}/accounts/{}/d1/database/{}/query", self.account, self.db);
        let mut resp = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .send_json(body)
            .map_err(|e| format!("D1: {e}"))?;
        let status = resp.status();
        let text = resp.body_mut().with_config().limit(256 << 20).read_to_string().map_err(|e| format!("D1: {e}"))?;
        let v: Value = serde_json::from_str(&text).map_err(|_| format!("D1 HTTP {status}: {text:.300}"))?;
        if !status.is_success() || v["success"] != json!(true) {
            return Err(format!("D1 HTTP {status}: {}", v["errors"]));
        }
        let results = v["result"].as_array().cloned().unwrap_or_default();
        if let Some(bad) = results.iter().find(|r| r["success"] == json!(false)) {
            return Err(format!("D1 statement failed: {bad}"));
        }
        Ok(results)
    }

    /// Rows of one statement.
    fn all<T: DeserializeOwned>(&self, q: Q) -> Res<Vec<T>> {
        let results = self.d1(&statement(q))?;
        let rows = results.into_iter().next().map(|r| r["results"].clone()).unwrap_or(json!([]));
        serde_json::from_value(rows).map_err(|e| format!("D1 rows: {e}"))
    }

    fn first<T: DeserializeOwned>(&self, q: Q) -> Res<Option<T>> {
        Ok(self.all(q)?.into_iter().next())
    }

    /// Several statements in one transaction.
    fn batch(&self, qs: Vec<Q>) -> Res<()> {
        self.d1(&json!({ "batch": qs.into_iter().map(statement).collect::<Vec<_>>() })).map(drop)
    }

    fn put(&self, key: &str, body: &[u8], content_type: &str, cache_control: Option<&str>) -> Res<()> {
        let url = format!("{API}/accounts/{}/r2/buckets/{}/objects/{}", self.account, self.bucket, object_path(key));
        let mut req = self
            .agent
            .put(&url)
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", content_type);
        if let Some(cc) = cache_control {
            req = req.header("Cache-Control", cc);
        }
        let mut resp = req.send(body).map_err(|e| format!("R2 put {key}: {e}"))?;
        if !resp.status().is_success() {
            let text = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(format!("R2 put {key}: HTTP {}: {text:.300}", resp.status()));
        }
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Same as `jobs::build_release` in the Worker: builds `releases/{lang}/{version}/` from the current translations,
/// signs the manifest and makes it active; unchanged content is skipped unless `force`.
fn build_release(cf: &Cf, key: &ed25519_dalek::SigningKey, lang: &str, force: bool) -> Res<String> {
    #[derive(Deserialize)]
    struct Kv {
        value: String,
    }
    if let Some(pin) = cf.first::<Kv>(q::kv_get(&format!("release_pin:{lang}")))? {
        return Ok(format!("{lang}: pinned to version {} by a rollback (build it from the admin API)", pin.value));
    }
    #[derive(Deserialize)]
    struct Row {
        text_key: String,
        en: String,
        tr: String,
        source: String,
        ids: Option<String>,
    }
    let mut builder = Builder::default();
    let mut after = String::new();
    loop {
        let rows: Vec<Row> = cf.all(q::release_page(lang, &after, RELEASE_PAGE))?;
        let Some(last) = rows.last() else { break };
        after = last.text_key.clone();
        for r in &rows {
            // Same rules the mod applies when loading.
            if validate::unusable(&r.en, &r.tr) {
                continue;
            }
            builder.push(&DictRecord {
                en: r.en.clone(),
                tr: r.tr.clone(),
                src: release_src(&r.source).into(),
                ids: parse_ids(r.ids.as_deref()),
            });
        }
        if (rows.len() as u64) < RELEASE_PAGE {
            break;
        }
    }
    let records = builder.count;
    let (files, content_hash) = builder.finish();

    #[derive(Deserialize)]
    struct Active {
        version: i64,
        content_hash: String,
    }
    if let Some(a) = cf.first::<Active>(q::active_release(lang))?
        && a.content_hash == content_hash
        && !force
    {
        return Ok(format!("{lang}: unchanged (version {}, {records} records)", a.version));
    }
    #[derive(Deserialize)]
    struct Max {
        v: Option<i64>,
    }
    let version = cf.first::<Max>(q::max_version(lang))?.and_then(|m| m.v).unwrap_or(0) as u64 + 1;
    let created_at = time::rfc3339(now_ms());
    let m = release::signed_manifest(lang, version, &created_at, &files, key);
    let manifest_json = serde_json::to_string(&m).map_err(|e| e.to_string())?;

    for c in &files {
        let k = release::r2_key(lang, version, &c.entry.name);
        cf.put(&k, &c.bytes, "application/gzip", Some("public, max-age=31536000, immutable"))?;
    }
    cf.put(&release::r2_key(lang, version, "manifest.json"), manifest_json.as_bytes(), "application/json", None)?;
    let files_json = serde_json::to_string(&m.files).map_err(|e| e.to_string())?;
    // Made active only once every file is in the bucket.
    cf.batch(q::release_insert(&q::NewRelease {
        lang,
        version: version as i64,
        created_at: &created_at,
        files: &files_json,
        manifest: &manifest_json,
        content_hash: &content_hash,
        records: records as i64,
    }))?;
    Ok(format!("{lang}: built version {version} ({records} records, {} files)", files.len()))
}

/// Same as `jobs::backup` in the Worker: every table to `backups/{timestamp}/{table}-NNN.jsonl.gz`, then `done.json`.
/// D1 rows written by seeding one record: texts (+1 index), translations (+2), current (+3), and per game id an
/// id_pairs row (+1).
fn seed_cost(ids: usize) -> u64 {
    2 + 3 + 4 + 2 * ids as u64
}

/// Records per D1 request (each is 3 statements plus one for its ids).
const SEED_CHUNK: usize = 100;

#[derive(Deserialize)]
struct SeedLine {
    en: String,
    tr: String,
    #[serde(default)]
    ids: Vec<u32>,
}

/// The statements that insert one dictionary record as trusted and current (source `ai`, already reviewed).
fn seed_statements(lang: &str, r: &SeedLine, now: &str) -> Vec<Value> {
    let key = hash::text_key(&r.en);
    let id = hash::translation_id(&key, lang, &r.tr);
    let mut s = vec![
        json!({"sql": "INSERT INTO texts (key, en, state, reporters, created_at, reviewed_at) VALUES (?, ?, 'trusted', 0, ?, ?)                        ON CONFLICT(key) DO UPDATE SET state = 'trusted', reviewed_at = excluded.reviewed_at WHERE state = 'pending'",
               "params": [key, r.en, now, now]}),
        json!({"sql": "INSERT OR IGNORE INTO translations (id, text_key, lang, tr, source, review, created_at, passed_at)                        VALUES (?, ?, ?, ?, 'ai', 'passed', ?, ?)",
               "params": [id, key, lang, r.tr, now, now]}),
        json!({"sql": "INSERT OR IGNORE INTO current (text_key, lang, translation_id, score, up, down, updated_at) VALUES (?, ?, ?, 0, 0, 0, ?)",
               "params": [key, lang, id, now]}),
    ];
    // D1 binds at most 100 parameters per statement: 4 per pair.
    for ids in r.ids.chunks(24) {
        let rows = vec!["(?, ?, 'trusted', 0, ?, ?)"; ids.len()].join(", ");
        let params: Vec<Value> = ids.iter().flat_map(|g| [json!(g), json!(key), json!(now), json!(now)]).collect();
        s.push(json!({"sql": format!("INSERT OR IGNORE INTO id_pairs (game_id, text_key, state, reporters, created_at, reviewed_at) VALUES {rows}"),
                      "params": params}));
    }
    s
}

#[derive(Deserialize)]
struct KvValue {
    value: String,
}

/// A SQL literal of a statement parameter (strings with quotes doubled, numbers as they are).
fn sql_literal(v: &Value) -> String {
    match v {
        // D1's file import cuts a statement at a raw line break inside a string, and a long `||` chain passes SQLite's
        // expression depth limit: line breaks travel as private-use placeholders put back by one replace() each.
        Value::String(s) if s.contains(['\n', '\r']) => format!(
            "replace(replace('{}', char(57344), char(10)), char(57345), char(13))",
            s.replace('\'', "''").replace('\n', "\u{E000}").replace('\r', "\u{E001}")
        ),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        Value::Number(n) => n.to_string(),
        Value::Null => "NULL".into(),
        other => format!("'{}'", other.to_string().replace('\'', "''")),
    }
}

/// One statement with its `?` parameters inlined (no `?` ever appears inside the SQL text of the seed statements).
fn inline_sql(stmt: &Value) -> String {
    let sql = stmt["sql"].as_str().unwrap_or_default();
    let params = stmt["params"].as_array().cloned().unwrap_or_default();
    let mut out = String::with_capacity(sql.len() + 64);
    let mut it = params.iter();
    for c in sql.chars() {
        if c == '?' {
            out.push_str(&it.next().map(sql_literal).unwrap_or_else(|| "NULL".into()));
        } else {
            out.push(c);
        }
    }
    out.push_str(";\n");
    out
}

/// `jobs seed-sql <dictionary.jsonl> <lang> <out prefix> [from line]`: the same seed as `jobs seed`, written as SQL files
/// of at most 50k statements (`<prefix>-NNN.sql`) for `wrangler d1 execute --remote --file`, for a machine where the jobs
/// binary cannot reach the API. The last file marks the seed done and refreshes the catalog size and stats.
fn seed_sql(path: &str, lang: &str, prefix: &str, from: usize) -> Res<String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let now = time::rfc3339(now_ms());
    let (mut files, mut buf, mut n, mut loaded, mut catalog) = (0usize, String::new(), 0usize, 0usize, 0usize);
    let flush = |buf: &mut String, files: &mut usize| -> Res<()> {
        if !buf.is_empty() {
            std::fs::write(format!("{prefix}-{:03}.sql", *files), buf.as_bytes()).map_err(|e| e.to_string())?;
            *files += 1;
            buf.clear();
        }
        Ok(())
    };
    for (i, l) in lines.iter().enumerate() {
        let r: SeedLine = serde_json::from_str(l).map_err(|e| format!("{path}:{}: {e}", i + 1))?;
        if validate::unusable(&r.en, &r.tr) {
            continue;
        }
        catalog += 1;
        if i < from {
            continue;
        }
        for s in seed_statements(lang, &r, &now) {
            buf.push_str(&inline_sql(&s));
            n += 1;
        }
        loaded += 1;
        if n >= 50_000 {
            flush(&mut buf, &mut files)?;
            n = 0;
        }
    }
    for s in [q::kv_put(&format!("catalog:{lang}"), &catalog.to_string(), &now), q::kv_put(&format!("seed:{lang}"), "done", &now), q::kv_delete(&format!("stats:{lang}"))] {
        buf.push_str(&inline_sql(&statement(s)));
    }
    flush(&mut buf, &mut files)?;
    Ok(format!("{lang}: {loaded} records from line {from} in {files} files ({prefix}-NNN.sql)"))
}

fn seed_pending(cf: &Cf, lang: &str) -> Res<bool> {
    Ok(cf.first::<KvValue>(q::kv_get(&format!("seed:{lang}")))?.is_some_and(|v| v.value != "done"))
}

fn seed(cf: &Cf, path: &str, lang: &str, budget: u64) -> Res<String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let kv_key = format!("seed:{lang}");
    let start = match cf.first::<KvValue>(q::kv_get(&kv_key))? {
        Some(v) if v.value == "done" => return Ok(format!("{lang}: seed already done")),
        Some(v) => v.value.parse::<usize>().map_err(|_| format!("bad {kv_key} value {:?}", v.value))?,
        None => 0,
    };
    let now = time::rfc3339(now_ms());
    // The whole dictionary's size, so the site's percentage is over every text and not only the loaded ones; the
    // cached stats are dropped so the next request recounts.
    let catalog = lines
        .iter()
        .filter_map(|l| serde_json::from_str::<SeedLine>(l).ok())
        .filter(|r| !validate::unusable(&r.en, &r.tr))
        .count();
    cf.batch(vec![q::kv_put(&format!("catalog:{lang}"), &catalog.to_string(), &now), q::kv_delete(&format!("stats:{lang}"))])?;
    let (mut i, mut spent, mut loaded) = (start, 0u64, 0usize);
    while i < lines.len() {
        let mut stmts = Vec::new();
        let mut next = i;
        while next < lines.len() && next - i < SEED_CHUNK {
            let r: SeedLine = serde_json::from_str(lines[next]).map_err(|e| format!("{path}:{}: {e}", next + 1))?;
            let cost = seed_cost(r.ids.len());
            if spent + cost > budget {
                break;
            }
            if !validate::unusable(&r.en, &r.tr) {
                stmts.extend(seed_statements(lang, &r, &now));
                spent += cost;
                loaded += 1;
            }
            next += 1;
        }
        if next == i {
            break;
        }
        if !stmts.is_empty() {
            cf.d1(&json!({ "batch": stmts }))?;
        }
        i = next;
        // Saved after every request, so a failed run resumes without redoing (or double counting) anything.
        let value = if i >= lines.len() { "done".to_string() } else { i.to_string() };
        cf.batch(vec![q::kv_put(&kv_key, &value, &now)])?;
    }
    let left = lines.len() - i;
    Ok(format!("{lang}: seeded {loaded} records (lines {start}..{i} of {}, ~{spent} rows written), {left} lines left", lines.len()))
}

fn backup(cf: &Cf) -> Res<String> {
    let prefix = format!("backups/{}", time::compact(now_ms()));
    let (mut total_rows, mut total_files) = (0usize, 0usize);
    for table in BackupTable::ALL {
        let name = table.name();
        let (mut after, mut part, mut rows_in_part) = (0i64, 0usize, 0usize);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let rows: Vec<Value> = cf.all(q::backup_page(table, after, BACKUP_PAGE))?;
            for r in &rows {
                serde_json::to_writer(&mut buf, r).map_err(|e| e.to_string())?;
                buf.push(b'\n');
            }
            rows_in_part += rows.len();
            total_rows += rows.len();
            let last = rows.last().and_then(|r| r.get("_rowid")).and_then(Value::as_f64).map(|v| v as i64);
            let finished = (rows.len() as u64) < BACKUP_PAGE;
            if rows_in_part >= BACKUP_PART_ROWS || (finished && (rows_in_part > 0 || part == 0)) {
                cf.put(&format!("{prefix}/{name}-{part:03}.jsonl.gz"), &gzip(&buf), "application/gzip", None)?;
                total_files += 1;
                part += 1;
                buf.clear();
                rows_in_part = 0;
            }
            match last {
                Some(l) if !finished => after = l,
                _ => break,
            }
        }
    }
    let tables: Vec<String> = BackupTable::ALL.iter().map(|t| t.name()).collect();
    let done = json!({ "rows": total_rows, "files": total_files, "tables": tables }).to_string();
    cf.put(&format!("{prefix}/done.json"), done.as_bytes(), "application/json", None)?;
    Ok(format!("backup {prefix}: {total_rows} rows in {total_files} files"))
}

fn run() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let force = args.iter().any(|a| a == "--force");
    if args.first().map(String::as_str) == Some("seed-sql") {
        let usage = "usage: jobs seed-sql <dictionary.jsonl> <lang> <out prefix> [from line]";
        let (path, lang, prefix) = (args.get(1).ok_or(usage)?, args.get(2).ok_or(usage)?, args.get(3).ok_or(usage)?);
        let from = args.get(4).map(|v| v.parse::<usize>().map_err(|e| format!("from line: {e}"))).transpose()?.unwrap_or(0);
        println!("{}", seed_sql(path, lang, prefix, from)?);
        return Ok(());
    }
    let cf = Cf::from_env()?;
    match args.first().map(String::as_str) {
        Some("release") => {
            let key = manifest::signing_key(&env("RELEASE_SIGNING_KEY", None)?)?;
            let langs = env("LANGS", Some(DEFAULT_LANGS))?;
            for lang in langs.split(',').map(str::trim).filter(|l| !l.is_empty()) {
                if seed_pending(&cf, lang)? {
                    println!("{lang}: waiting for the dictionary seed to finish");
                    continue;
                }
                println!("{}", build_release(&cf, &key, lang, force)?);
            }
        }
        Some("backup") => println!("{}", backup(&cf)?),
        Some("seed") => {
            let path = args.get(1).ok_or("usage: jobs seed <dictionary.jsonl> [lang]")?;
            let lang = args.get(2).map(String::as_str).unwrap_or(DEFAULT_LANGS);
            let budget = env("SEED_BUDGET", Some("75000"))?.parse::<u64>().map_err(|e| format!("SEED_BUDGET: {e}"))?;
            println!("{}", seed(&cf, path, lang, budget)?);
        }
        _ => return Err("usage: jobs release [--force] | jobs backup | jobs seed <dictionary.jsonl> [lang]".into()),
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_keep_slashes_and_escape_the_rest() {
        assert_eq!(object_path("releases/pt-BR/3/dictionary-000.jsonl.gz"), "releases/pt-BR/3/dictionary-000.jsonl.gz");
        assert_eq!(object_path("a b/ç?"), "a%20b/%C3%A7%3F");
    }

    #[test]
    fn seed_statements_insert_text_translation_current_and_ids() {
        let r = SeedLine { en: "Hello".into(), tr: "Olá".into(), ids: (1..=30).collect() };
        let s = seed_statements("pt-BR", &r, "2026-11-15T00:00:00Z");
        // texts, translations, current, then the 30 ids in two statements (24 + 6).
        assert_eq!(s.len(), 5);
        let key = hash::text_key("Hello");
        assert_eq!(s[0]["params"][0], json!(key));
        assert_eq!(s[1]["params"][0], json!(hash::translation_id(&key, "pt-BR", "Olá")));
        assert_eq!(s[3]["params"].as_array().unwrap().len(), 96);
        assert_eq!(s[4]["params"].as_array().unwrap().len(), 24);
        for st in &s {
            let sql = st["sql"].as_str().unwrap();
            assert_eq!(sql.matches('?').count(), st["params"].as_array().unwrap().len(), "{sql}");
        }
        assert_eq!(seed_cost(30), 69);
    }

    #[test]
    fn statements_bind_json_params() {
        let s = statement(q::max_version("pt-BR"));
        assert_eq!(s["params"], json!(["pt-BR"]));
        assert!(s["sql"].as_str().unwrap().contains('?'));
    }
}
