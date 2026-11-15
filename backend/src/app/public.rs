//! Public endpoints (mod and site). No authentication: everything is checked.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use worker::*;

use super::db::{self, ensure_not_banned, ensure_rate};
use super::external::verify_turnstile;
use super::{Ctx, Fail, HResult, api_err, apply_headers, body, json};
use crate::api::{self, CorrectionRequest, Sort, VoteRequest};
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
    ensure_rate(ctx, &ratelimit::CONTRIBUTIONS, &who.ip, prepared.total.max(1) as f64).await?;
    if !prepared.items.is_empty() {
        db::store_contribution(ctx, &prepared, &who.ip).await?;
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

/// `GET /v1/texts?lang=&sort=worst|best|recent&q=&cursor=`
pub async fn texts(req: &mut Request, ctx: &Ctx) -> HResult {
    let url = req.url()?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let tq = api::parse_texts_query(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())), &ctx.langs())?;
    if let Ok(who) = ctx.client(req) {
        ensure_rate(ctx, &ratelimit::READS, &who.ip, 1.0).await?;
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
    };
    let pattern = tq.q.as_deref().map(api::like_pattern);
    let rows: Vec<Row> = ctx
        .q(q::list_texts(
            &tq.lang,
            sort,
            pattern.as_deref(),
            tq.limit as u64 + 1,
            tq.offset as u64,
        ))?
        .all()
        .await?
        .results()?;
    let more = rows.len() > tq.limit as usize;
    let rows = &rows[..rows.len().min(tq.limit as usize)];
    let keys = json!(rows.iter().map(|r| &r.key).collect::<Vec<_>>()).to_string();
    let trs: Vec<TrOut> = ctx.q(q::passed_translations(&tq.lang, &keys))?.all().await?.results()?;
    #[derive(Deserialize)]
    struct Ids {
        text_key: String,
        ids: Option<String>,
    }
    let ids: Vec<Ids> = ctx.q(q::trusted_ids(&keys))?.all().await?.results()?;
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
    let next = more.then(|| (tq.offset + tq.limit).to_string());
    json(200, &json!({ "lang": tq.lang, "items": items, "next_cursor": next }))
}

/// `POST /v1/votes`
pub async fn vote(req: &mut Request, ctx: &Ctx) -> HResult {
    let who = ctx.client(req)?;
    let v = VoteRequest::parse(&body(req, 8 * 1024).await?)?;
    ensure_not_banned(ctx, &who).await?;
    ensure_rate(ctx, &ratelimit::VOTES, &who.ip, 1.0).await?;
    verify_turnstile(ctx, req, &v.turnstile).await?;
    let Some((text_key, lang, review)) = db::translation_target(ctx, &v.translation_id).await? else {
        return Err(api_err(404, "unknown_translation"));
    };
    if review != "passed" {
        return Err(api_err(409, "not_votable"));
    }
    let res = ctx
        .q(q::vote_insert(&v.translation_id, &who.ip, v.value, &ctx.now()))?
        .run()
        .await?;
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
    ensure_rate(ctx, &ratelimit::CORRECTIONS, &who.ip, 1.0).await?;
    verify_turnstile(ctx, req, &c.turnstile).await?;
    let id = translation_id(&c.text_key, &c.lang, &c.tr);
    ctx.db
        .batch(ctx.qs(q::correction(&id, &c.text_key, &c.lang, &c.tr, &who.ip, &ctx.now()))?)
        .await?;
    let (_, _, review) = db::translation_target(ctx, &id)
        .await?
        .ok_or_else(|| api_err(500, "internal"))?;
    json(202, &json!({ "translation_id": id, "review": review }))
}

/// `GET /v1/stats?lang=`
pub async fn stats(req: &mut Request, ctx: &Ctx) -> HResult {
    let url = req.url()?;
    let lang = url
        .query_pairs()
        .find(|(k, _)| k == "lang")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default();
    if !validate::lang_allowed(&lang, &ctx.langs()) {
        return Err(api_err(400, "unsupported_lang"));
    }
    let mut v = match super::jobs::cached_stats(ctx, &lang).await? {
        Some(v) => v,
        None => super::jobs::compute_stats(ctx, &lang).await?,
    };
    v["downloads"] = json!(db::counter(ctx, "downloads").await?);
    json(200, &v)
}

/// `GET /v1/download`: counts (at most a few per IP per day), then redirects.
pub async fn download(req: &mut Request, ctx: &Ctx) -> HResult {
    if let Ok(who) = ctx.client(req) {
        let d = db::rate(ctx, &ratelimit::DOWNLOADS, &who.ip, 1.0).await?;
        if d.allowed {
            ctx.q(q::counter_increment("downloads"))?.run().await?;
        }
    }
    let target = ctx.var(
        "DOWNLOAD_URL",
        "https://github.com/wilssola/GW2Enhanced/releases/latest",
    );
    let mut r = Response::redirect_with_status(Url::parse(&target).map_err(Error::from)?, 302)?;
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
pub async fn release_file(ctx: &Ctx, lang: &str, version: &str, name: &str) -> HResult {
    let version: u64 = version.parse().map_err(|_| api_err(404, "not_found"))?;
    if !validate::lang_allowed(lang, &ctx.langs()) || !valid_file_name(name) {
        return Err(api_err(404, "not_found"));
    }
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
