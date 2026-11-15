//! The backend's daily jobs, run by GitHub Actions (`.github/workflows/backend-jobs.yml`) instead of the Worker:
//! building a dictionary release gzips and hashes the whole dictionary and a backup exports every table, which does not
//! fit the CPU time a Worker gets on the Free plan. Both talk to the same D1 database and R2 bucket through the
//! Cloudflare API and use the same statements (`crate::queries`) as the Worker.
//!
//! ```text
//! jobs release [--force]   build every language in LANGS (skips unchanged and pinned ones)
//! jobs backup              export every table to backups/<timestamp>/
//! ```
//! Env: `CLOUDFLARE_API_TOKEN` (D1 edit + R2 edit), `CLOUDFLARE_ACCOUNT_ID`, `RELEASE_SIGNING_KEY` (release only);
//! optional `D1_DATABASE_ID`, `R2_BUCKET`, `LANGS` (defaults: the values in wrangler.toml).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gw2translated_backend::chunk::gzip;
use gw2translated_backend::queries::{self as q, BackupTable, Q};
use gw2translated_backend::release::{self, Builder, DictRecord, parse_ids, release_src};
use gw2translated_backend::{manifest, time, validate};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const DEFAULT_DB: &str = "f3ceb6c1-9476-4971-b098-4eddc75fc8ef";
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
    let cf = Cf::from_env()?;
    match args.first().map(String::as_str) {
        Some("release") => {
            let key = manifest::signing_key(&env("RELEASE_SIGNING_KEY", None)?)?;
            let langs = env("LANGS", Some(DEFAULT_LANGS))?;
            for lang in langs.split(',').map(str::trim).filter(|l| !l.is_empty()) {
                println!("{}", build_release(&cf, &key, lang, force)?);
            }
        }
        Some("backup") => println!("{}", backup(&cf)?),
        _ => return Err("usage: jobs release [--force] | jobs backup".into()),
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
    fn statements_bind_json_params() {
        let s = statement(q::max_version("pt-BR"));
        assert_eq!(s["params"], json!(["pt-BR"]));
        assert!(s["sql"].as_str().unwrap().contains('?'));
    }
}
