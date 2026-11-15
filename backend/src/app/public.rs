//! Public endpoints (mod and site). No authentication: everything is checked.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::*;

use super::db::{self, ensure_not_banned, ensure_rate, ensure_read_rate, ensure_writes};
use super::external::verify_turnstile;
use super::{Ctx, Fail, HResult, api_err, apply_headers, body, json};
use crate::api::{self, CorrectionRequest, Sort, VoteRequest};
use crate::budget;
use crate::chunk::{GunzipError, gunzip_limited, is_gzip};
use crate::hash::translation_id;
use crate::headers::{Kind, escape_html};
use crate::queries::{self as q, ListSort};
use crate::ratelimit;
use crate::release::{self, parse_ids};
use crate::score;
use crate::validate;

const SITE: &str = include_str!("../../site/index.html");
const APP_JS: &str = include_str!("../../site/app.js");
const APP_CSS: &str = include_str!("../../site/app.css");

/// Self-hosted fonts (SIL OFL 1.1, see `site/fonts/OFL.txt`).
const FONTS: [(&str, &[u8]); 5] = [
    ("cinzel.woff2", include_bytes!("../../site/fonts/cinzel.woff2")),
    (
        "eb-garamond.woff2",
        include_bytes!("../../site/fonts/eb-garamond.woff2"),
    ),
    (
        "eb-garamond-italic.woff2",
        include_bytes!("../../site/fonts/eb-garamond-italic.woff2"),
    ),
    (
        "barlow-condensed-500.woff2",
        include_bytes!("../../site/fonts/barlow-condensed-500.woff2"),
    ),
    (
        "barlow-condensed-600.woff2",
        include_bytes!("../../site/fonts/barlow-condensed-600.woff2"),
    ),
];

/// `GET /`: static HTML; the only server-side value is the Turnstile site key
/// (an env var), HTML-escaped and restricted to `[A-Za-z0-9_-]`.
pub fn site(ctx: &Ctx) -> HResult {
    let key: String = ctx
        .var("TURNSTILE_SITE_KEY", "")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    // The script and style URLs carry a hash of their content, so a browser never mixes a new page with a cached old
    // stylesheet or script after an update.
    let v = |s: &str| crate::hash::sha256_hex(s.as_bytes())[..12].to_string();
    let html = SITE
        .replace("__TURNSTILE_SITE_KEY__", &escape_html(&key))
        .replace("\"/app.css\"", &format!("\"/app.css?v={}\"", v(APP_CSS)))
        .replace("\"/app.js\"", &format!("\"/app.js?v={}\"", v(APP_JS)));
    let mut r = Response::from_html(html)?;
    r.headers_mut().set("cache-control", "public, max-age=300")?;
    apply_headers(&mut r, Kind::Html)?;
    Ok(r)
}

pub enum Asset {
    Js,
    Css,
}

/// `GET /app.js`, `GET /app.css`.
pub fn asset(a: Asset) -> HResult {
    let (body, ctype) = match a {
        Asset::Js => (APP_JS, "text/javascript; charset=utf-8"),
        Asset::Css => (APP_CSS, "text/css; charset=utf-8"),
    };
    let mut r = Response::ok(body)?;
    r.headers_mut().set("content-type", ctype)?;
    r.headers_mut().set("cache-control", "public, max-age=3600")?;
    apply_headers(&mut r, Kind::Asset)?;
    Ok(r)
}

/// `GET /fonts/{name}`.
pub fn font(name: &str) -> HResult {
    let (_, bytes) = FONTS
        .iter()
        .find(|(n, _)| *n == name)
        .ok_or_else(|| api_err(404, "not_found"))?;
    let mut r = Response::from_bytes(bytes.to_vec())?;
    r.headers_mut().set("content-type", "font/woff2")?;
    r.headers_mut()
        .set("cache-control", "public, max-age=31536000, immutable")?;
    apply_headers(&mut r, Kind::Asset)?;
    Ok(r)
}

/// `GET /img/{name}` (see [`crate::images`]).
pub fn image(name: &str) -> HResult {
    let (bytes, ctype) = crate::images::image(name).ok_or_else(|| api_err(404, "not_found"))?;
    let mut r = Response::from_bytes(bytes.to_vec())?;
    r.headers_mut().set("content-type", ctype)?;
    r.headers_mut().set("cache-control", "public, max-age=86400")?;
    apply_headers(&mut r, Kind::Asset)?;
    Ok(r)
}

/// Worst-case rows written by storing `p`, and the budget units of the text it carries.
fn budget_units(p: &api::Prepared) -> (u64, u64) {
    let pairs = p.items.iter().filter(|i| i.game_id.is_some()).count();
    let trs = p.items.iter().filter(|i| i.tr.is_some()).count();
    let bytes: usize = p
        .items
        .iter()
        .map(|i| i.en.len() + i.tr.as_ref().map_or(0, |t| t.tr.len()))
        .sum();
    (budget::contribution_rows(p.items.len(), pairs, trs), budget::bytes_units(bytes))
}

/// `POST /v1/contributions`
pub async fn contributions(req: &mut Request, ctx: &Ctx) -> HResult {
    let who = ctx.client(req)?;
    ensure_not_banned(ctx, &who).await?;
    let raw = body(req, api::MAX_BODY_BYTES).await?;
    // The runtime may or may not have decoded `Content-Encoding: gzip`
    // already: trust the bytes, not the header.
    let data = if is_gzip(&raw) {
        gunzip_limited(&raw, api::MAX_BODY_BYTES).map_err(|e| match e {
            GunzipError::TooLarge => api_err(413, "body_too_large"),
            GunzipError::Invalid => api_err(400, "bad_gzip"),
        })?
    } else {
        raw
    };
    let prepared = api::prepare_contribution(&data, &ctx.langs())?;
    let limits = [(&ratelimit::CONTRIBUTIONS, who.ip.as_str()), (&ratelimit::CONTRIBUTIONS_NET, who.net.as_str())];
    let buckets = ensure_rate(ctx, &limits, prepared.total.max(1) as f64).await?;
    // Reserve the worst case (every item new) plus the text's size, then settle on what D1 reports it wrote.
    let (estimate, text_units) = budget_units(&prepared);
    ensure_writes(ctx, estimate + text_units).await?;
    ctx.batch(buckets.statements(ctx)?).await?;
    if !prepared.items.is_empty() {
        db::store_contribution(ctx, &prepared, &who.ip).await?;
    }
    if let Some(written) = ctx.rows_written() {
        // +1: the settling write itself.
        let delta = (written + 1) as i64 - estimate as i64;
        if let Err(e) = db::charge_writes(ctx, delta).await {
            console_warn!("write budget not settled: {e}");
        }
    }
    json(
        202,
        &json!({ "accepted": prepared.accepted(), "rejected": prepared.rejected }),
    )
}

#[derive(Serialize, Deserialize, Clone)]
struct TrOut {
    id: String,
    #[serde(skip_serializing, default)]
    text_key: String,
    tr: String,
    source: String,
    up: i64,
    down: i64,
    score: f64,
}

/// `GET /v1/texts?lang=&sort=recent|oldest|best|worst&page=&limit=&q=` (`cursor=` is the old offset form).
///
/// Pages without `q` are cached at the edge for [`TEXTS_CACHE_SECS`] under a canonical URL, so walking deep pages
/// again does not re-read D1.
pub async fn texts(req: &mut Request, ctx: &Ctx) -> HResult {
    let url = req.url()?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let tq = api::parse_texts_query(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())), &ctx.langs())?;
    let cache_key = api::texts_cache_key(&url.origin().ascii_serialization(), &tq);
    let cache = Cache::default();
    if let Some(k) = &cache_key {
        if let Ok(Some(hit)) = cache.get(k, false).await {
            return Ok(hit);
        }
    }
    // Only a miss costs D1, so only a miss is counted. Searches and deep pages read far more rows, so they take more
    // tokens, and they stop first when the day's read budget runs low (cheap first pages keep the last fifth).
    if let Ok(who) = ctx.client(req) {
        let cost = ratelimit::read_cost(tq.q.is_some(), tq.offset);
        ensure_read_rate(ctx, &ratelimit::READS, &who.ip, cost)?;
    }
    let expensive = tq.q.is_some() || tq.offset >= EXPENSIVE_OFFSET;
    let tier = if expensive { budget::Tier::Public } else { budget::Tier::Reserve };
    if !db::reads_allowed(ctx, tier).await? {
        let retry = budget::seconds_to_midnight(ctx.now_ms);
        let mut r = json(503, &json!({ "error": "read_budget_exhausted", "retry_after": retry }))?;
        r.headers_mut().set("retry-after", &retry.to_string())?;
        return Ok(r);
    }
    #[derive(Deserialize)]
    struct Row {
        key: String,
        en: String,
        cur: Option<String>,
    }
    let sort = match tq.sort {
        Sort::Worst => ListSort::Worst,
        Sort::Best => ListSort::Best,
        Sort::Recent => ListSort::Recent,
        Sort::Oldest => ListSort::Oldest,
    };
    let pattern = tq.q.as_deref().map(api::like_pattern);
    let rows: Vec<Row> = ctx
        .all(ctx.q(q::list_texts(
            &tq.lang,
            sort,
            pattern.as_deref(),
            tq.limit as u64 + 1,
            tq.offset as u64,
        ))?)
        .await?
        .results()?;
    let more = rows.len() > tq.limit as usize;
    let rows = &rows[..rows.len().min(tq.limit as usize)];
    let keys = json!(rows.iter().map(|r| &r.key).collect::<Vec<_>>()).to_string();
    let trs: Vec<TrOut> = ctx.all(ctx.q(q::passed_translations(&tq.lang, &keys))?).await?.results()?;
    #[derive(Deserialize)]
    struct Ids {
        text_key: String,
        ids: Option<String>,
    }
    let ids: Vec<Ids> = ctx.all(ctx.q(q::trusted_ids(&keys))?).await?.results()?;
    db::charge_reads(ctx, ctx.rows_read()).await;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mine: Vec<&TrOut> = trs.iter().filter(|t| t.text_key == r.key).collect();
            let current = mine.iter().find(|t| Some(&t.id) == r.cur.as_ref()).cloned();
            let alternatives: Vec<&TrOut> = mine.iter().filter(|t| Some(&t.id) != r.cur.as_ref()).copied().collect();
            let game_ids = parse_ids(ids.iter().find(|i| i.text_key == r.key).and_then(|i| i.ids.as_deref()));
            json!({ "text_key": r.key, "en": r.en, "ids": game_ids, "current": current, "alternatives": alternatives })
        })
        .collect();
    let max = if tq.q.is_some() { api::MAX_SEARCH_OFFSET } else { api::MAX_OFFSET };
    let next_offset = tq.offset + tq.limit;
    let more = more && next_offset <= max;
    let next = more.then(|| next_offset.to_string());
    // Every sort lists the same texts, so the cached count of texts with a translation is the total. Searches
    // have no total.
    let total = match tq.q {
        Some(_) => Value::Null,
        None => {
            let stats = super::jobs::stats_for(ctx, &tq.lang).await?;
            stats.get("translated").cloned().unwrap_or(Value::Null)
        }
    };
    let mut r = json(
        200,
        &json!({
            "lang": tq.lang, "sort": tq.sort.name(), "page": tq.offset / tq.limit + 1, "limit": tq.limit,
            "offset": tq.offset, "total": total, "more": more, "items": items, "next_cursor": next
        }),
    )?;
    if let Some(k) = cache_key {
        r.headers_mut()
            .set("cache-control", &format!("public, max-age=0, s-maxage={TEXTS_CACHE_SECS}"))?;
        if let Ok(copy) = r.cloned() {
            // A failed cache write only costs the next reader a D1 query.
            let _ = cache.put(&k, copy).await;
        }
    }
    Ok(r)
}

/// How long the edge keeps a page of `GET /v1/texts` (votes show up in the list after at most this long).
pub const TEXTS_CACHE_SECS: u32 = 300;

/// Pages from this offset on count as expensive reads (the index walk to reach them reads about 2 rows per skipped
/// text), like searches.
const EXPENSIVE_OFFSET: u32 = 1_000;

/// `POST /v1/votes`
pub async fn vote(req: &mut Request, ctx: &Ctx) -> HResult {
    let who = ctx.client(req)?;
    let v = VoteRequest::parse(&body(req, 8 * 1024).await?)?;
    ensure_not_banned(ctx, &who).await?;
    // Every attempt that reaches Turnstile spends its token, failed ones too, and the bucket is stored right away so
    // the gap between reading and storing it is one D1 round trip (a refused request still writes nothing).
    let limits = [(&ratelimit::VOTES, who.ip.as_str()), (&ratelimit::VOTES_NET, who.net.as_str())];
    let buckets = ensure_rate(ctx, &limits, 1.0).await?;
    ctx.batch(buckets.statements(ctx)?).await?;
    verify_turnstile(ctx, req, &v.turnstile).await?;
    let Some((text_key, lang, review)) = db::translation_target(ctx, &v.translation_id).await? else {
        return Err(api_err(404, "unknown_translation"));
    };
    if review != "passed" {
        return Err(api_err(409, "not_votable"));
    }
    ensure_writes(ctx, budget::VOTE_ROWS).await?;
    let stmts = vec![ctx.q(q::vote_insert(&v.translation_id, &who.ip, v.value, &ctx.now()))?];
    let res = ctx.batch(stmts).await?.pop().ok_or_else(|| api_err(500, "internal"))?;
    if res.meta()?.and_then(|m| m.changes).unwrap_or(0) == 0 {
        return Err(api_err(409, "already_voted"));
    }
    db::recount_votes(ctx, std::slice::from_ref(&v.translation_id)).await?;
    db::recompute_current(ctx, &[(text_key, lang)]).await?;
    #[derive(Deserialize)]
    struct Row {
        up: i64,
        down: i64,
        current: i64,
    }
    let r: Option<Row> = ctx.q(q::vote_result(&v.translation_id))?.first(None).await?;
    let r = r.ok_or_else(|| api_err(404, "unknown_translation"))?;
    json(
        200,
        &json!({
            "translation_id": v.translation_id, "up": r.up, "down": r.down,
            "score": score::wilson_lower_bound(r.up, r.down), "current": r.current != 0
        }),
    )
}

/// `POST /v1/corrections`
pub async fn correction(req: &mut Request, ctx: &Ctx) -> HResult {
    let who = ctx.client(req)?;
    let c = CorrectionRequest::parse(&body(req, 32 * 1024).await?, &ctx.langs())?;
    ensure_not_banned(ctx, &who).await?;
    #[derive(Deserialize)]
    struct Text {
        en: String,
        state: String,
    }
    let t: Option<Text> = ctx.q(q::text_get(&c.text_key))?.first(None).await?;
    let t = t
        .filter(|t| t.state == "trusted")
        .ok_or_else(|| api_err(404, "unknown_text"))?;
    validate::check_tr(&t.en, &c.tr).map_err(|e| api_err(422, e.as_str()))?;
    let limits = [(&ratelimit::CORRECTIONS, who.ip.as_str()), (&ratelimit::CORRECTIONS_NET, who.net.as_str())];
    let buckets = ensure_rate(ctx, &limits, 1.0).await?;
    verify_turnstile(ctx, req, &c.turnstile).await?;
    ensure_writes(ctx, budget::CORRECTION_ROWS + budget::bytes_units(c.tr.len())).await?;
    let id = translation_id(&c.text_key, &c.lang, &c.tr);
    let stmts = ctx.qs(q::correction(&id, &c.text_key, &c.lang, &c.tr, &who.ip, &ctx.now()))?;
    ctx.batch(stmts).await?;
    let (_, _, review) = db::translation_target(ctx, &id)
        .await?
        .ok_or_else(|| api_err(500, "internal"))?;
    json(202, &json!({ "translation_id": id, "review": review }))
}

/// `GET /v1/stats?lang=`
pub async fn stats(req: &mut Request, ctx: &Ctx) -> HResult {
    // As for votes: a failed attempt spends its token too.
    ctx.batch(buckets.statements(ctx)?).await?;
    let url = req.url()?;
    let lang = url
        .query_pairs()
        .find(|(k, _)| k == "lang")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    if !validate::lang_allowed(&lang, &ctx.langs()) {
        return Err(api_err(400, "unsupported_lang"));
    }
    let mut v = super::jobs::stats_for(ctx, &lang).await?;
    v["downloads"] = json!(db::counter(ctx, "downloads").await?);
    json(200, &v)
}

/// The installer of the latest release for a platform: `os=windows|linux|deb`, else guessed from the User-Agent
/// (Linux desktops get the AppImage, everyone else the Windows installer).
pub fn installer_asset(os: Option<&str>, user_agent: &str) -> &'static str {
    const WINDOWS: &str = "GW2Enhanced-Installer-windows-x64.exe";
    const APPIMAGE: &str = "GW2Enhanced-Installer-linux-amd64.AppImage";
    const DEB: &str = "GW2Enhanced-Installer-linux-amd64.deb";
    match os {
        Some("windows") => WINDOWS,
        Some("linux") => APPIMAGE,
        Some("deb") => DEB,
        _ if user_agent.contains("Linux") && !user_agent.contains("Android") => APPIMAGE,
        _ => WINDOWS,
    }
}

/// `GET /v1/download[?os=windows|linux|deb]`: counts (at most a few per network per day, within the write budget),
/// then redirects to the installer of the latest release.
pub async fn download(req: &mut Request, ctx: &Ctx) -> HResult {
    // The count is best effort: a database error or a spent budget never blocks the download. An uncounted download
    // writes nothing (a refused bucket is not stored).
    if let Ok(who) = ctx.client(req)
        && let Ok(Ok(buckets)) = db::rate_check(ctx, &[(&ratelimit::DOWNLOADS, who.net.as_str())], 1.0).await
        && let Ok(true) = db::reserve_writes(ctx, budget::DOWNLOAD_ROWS, budget::Tier::Public).await
        && let Ok(mut stmts) = buckets.statements(ctx)
        && let Ok(stmt) = ctx.q(q::counter_increment("downloads"))
    {
        stmts.push(stmt);
        let _ = ctx.batch(stmts).await;
    }
    let url = req.url()?;
    let os = url.query_pairs().find(|(k, _)| k == "os").map(|(_, v)| v.into_owned());
    let ua = req.headers().get("user-agent").ok().flatten().unwrap_or_default();
    let base = ctx.var(
        "DOWNLOAD_URL",
        "https://github.com/wilssola/GW2Enhanced/releases/latest/download",
    );
    let target = format!("{}/{}", base.trim_end_matches('/'), installer_asset(os.as_deref(), &ua));
    // Response::redirect has immutable headers in Workers (setting cache-control on it throws): build the 302 by hand.
    let target = Url::parse(&target).map_err(Error::from)?;
    let mut r = Response::empty()?.with_status(302);
    r.headers_mut().set("location", target.as_str())?;
    r.headers_mut().set("cache-control", "no-store")?;
    apply_headers(&mut r, Kind::Asset)?;
    Ok(r)
}

/// `GET /v1/public-key`: the release signing public key (raw 32 bytes, base64).
pub fn public_key(ctx: &Ctx) -> HResult {
    let key = ctx
        .secret("RELEASE_SIGNING_KEY")
        .ok_or_else(|| api_err(503, "signing_not_configured"))?;
    let key = crate::manifest::signing_key(&key).map_err(|e| Fail::Internal(Error::RustError(e)))?;
    json(
        200,
        &json!({ "algorithm": "ed25519", "public_key": crate::manifest::public_key_b64(&key) }),
    )
}

/// `GET /v1/releases/{lang}/latest`
pub async fn latest(ctx: &Ctx, lang: &str) -> HResult {
    if !validate::lang_allowed(lang, &ctx.langs()) {
        return Err(api_err(404, "unknown_lang"));
    }
    #[derive(Deserialize)]
    struct Row {
        manifest: String,
    }
    let r: Option<Row> = ctx.q(q::active_manifest(lang))?.first(None).await?;
    let r = r.ok_or_else(|| api_err(404, "no_release"))?;
    let mut resp = Response::ok(r.manifest)?;
    resp.headers_mut().set("cache-control", "public, max-age=300")?;
    apply_headers(&mut resp, Kind::Json)?;
    Ok(resp)
}

fn valid_file_name(name: &str) -> bool {
    if name == "manifest.json" {
        return true;
    }
    let Some(rest) = name
        .strip_prefix(release::FILE_PREFIX)
        .and_then(|r| r.strip_prefix('-'))
    else {
        return false;
    };
    let Some(num) = rest.strip_suffix(".jsonl.gz") else {
        return false;
    };
    num.len() == 3 && num.bytes().all(|b| b.is_ascii_digit())
}

/// `GET /v1/releases/{lang}/{version}/{name}` (immutable).
///
/// Served from the edge cache when it can be: each R2 read is a billed class B operation. A miss first checks the
/// version in D1 (one row read, far cheaper), so random versions never reach R2.
pub async fn release_file(req: &Request, ctx: &Ctx, lang: &str, version: &str, name: &str) -> HResult {
    let version: u64 = version.parse().map_err(|_| api_err(404, "not_found"))?;
    if !validate::lang_allowed(lang, &ctx.langs()) || !valid_file_name(name) || version > i64::MAX as u64 {
        return Err(api_err(404, "not_found"));
    }
    let url = req.url()?;
    let key = format!("{}/v1/releases/{lang}/{version}/{name}", url.origin().ascii_serialization());
    let cache = Cache::default();
    if let Ok(Some(hit)) = cache.get(&key, false).await {
        return Ok(hit);
    }
    #[derive(Deserialize)]
    struct Exists {
        #[allow(dead_code)]
        version: i64,
    }
    let known: Option<Exists> = ctx.q(q::release_exists(lang, version as i64))?.first(None).await?;
    if known.is_none() {
        return Err(api_err(404, "not_found"));
    }
    let mut r = release_object(ctx, lang, version, name).await?;
    if let Ok(copy) = r.cloned() {
        let _ = cache.put(&key, copy).await;
    }
    Ok(r)
}

async fn release_object(ctx: &Ctx, lang: &str, version: u64, name: &str) -> HResult {
    let obj = ctx
        .bucket()?
        .get(release::r2_key(lang, version, name))
        .execute()
        .await?;
    let obj = obj.ok_or_else(|| api_err(404, "not_found"))?;
    let etag = obj.http_etag();
    let bytes = obj.body().ok_or_else(|| api_err(404, "not_found"))?.bytes().await?;
    let mut r = Response::from_bytes(bytes)?;
    let h = r.headers_mut();
    h.set(
        "content-type",
        if name.ends_with(".gz") {
            "application/gzip"
        } else {
            "application/json; charset=utf-8"
        },
    )?;
    h.set("cache-control", "public, max-age=31536000, immutable")?;
    h.set("etag", &etag)?;
    apply_headers(&mut r, Kind::Asset)?;
    Ok(r)
}
