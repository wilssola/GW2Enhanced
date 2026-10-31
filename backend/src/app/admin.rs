//! Admin endpoints, behind `Authorization: Bearer <ADMIN_TOKEN>`.

use serde::Deserialize;
use serde_json::{Value, json};
use worker::*;

use super::{Ctx, HResult, api_err, body, db, jobs, json};
use crate::hash::{self, ct_eq, is_key};
use crate::queries as q;
use crate::validate;

fn authorized(req: &Request, ctx: &Ctx) -> std::result::Result<(), super::Fail> {
    let token = ctx
        .secret("ADMIN_TOKEN")
        .ok_or_else(|| api_err(503, "admin_disabled"))?;
    let given = req.headers().get("authorization")?.unwrap_or_default();
    let given = given.strip_prefix("Bearer ").unwrap_or("");
    if token.len() < 16 || !ct_eq(given.as_bytes(), token.as_bytes()) {
        return Err(api_err(401, "unauthorized"));
    }
    Ok(())
}

async fn json_body(req: &mut Request) -> std::result::Result<Value, super::Fail> {
    let b = body(req, 16 * 1024).await?;
    if b.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&b).map_err(|_| api_err(400, "malformed_body"))
}

pub async fn route(req: &mut Request, ctx: &Ctx, rest: &[&str]) -> HResult {
    authorized(req, ctx)?;
    match (req.method(), rest) {
        (Method::Get, ["translations", id]) => translation(ctx, id).await,
        (Method::Post, ["translations", id, "hide"]) => {
            let b = json_body(req).await?;
            set_visibility(ctx, id, true, b.get("reason").and_then(Value::as_str)).await
        }
        (Method::Post, ["translations", id, "unhide"]) => set_visibility(ctx, id, false, None).await,
        (Method::Post, ["bans"]) => {
            let b = json_body(req).await?;
            ban(ctx, &b).await
        }
        (Method::Delete, ["bans", h]) => {
            ctx.q(q::ban_delete(h))?.run().await?;
            json(200, &json!({ "ok": true }))
        }
        (Method::Get, ["releases", lang]) => releases(ctx, lang).await,
        (Method::Post, ["releases", lang, "build"]) => {
            let b = json_body(req).await?;
            ensure_lang(ctx, lang)?;
            // An explicit build ends a rollback pin.
            jobs::set_pin(ctx, lang, None).await?;
            let r = jobs::build_release(ctx, lang, b.get("force").and_then(Value::as_bool).unwrap_or(false)).await?;
            json(200, &r)
        }
        (Method::Post, ["releases", lang, "rollback"]) => {
            let b = json_body(req).await?;
            rollback(ctx, lang, b.get("version").and_then(Value::as_i64)).await
        }
        (Method::Post, ["review"]) => {
            let r = jobs::review(ctx).await?;
            json(200, &r)
        }
        (Method::Post, ["backup"]) => {
            let r = jobs::backup(ctx).await?;
            json(200, &r)
        }
        _ => Err(api_err(404, "not_found")),
    }
}

fn ensure_lang(ctx: &Ctx, lang: &str) -> std::result::Result<(), super::Fail> {
    if validate::lang_allowed(lang, &ctx.langs()) {
        Ok(())
    } else {
        Err(api_err(404, "unknown_lang"))
    }
}

/// Translation details with reporter and voter IP hashes (to find abusers).
async fn translation(ctx: &Ctx, id: &str) -> HResult {
    let t: Option<Value> = ctx.q(q::translation_details(id))?.first(None).await?;
    let t = t.ok_or_else(|| api_err(404, "unknown_translation"))?;
    let reporters: Vec<Value> = ctx.q(q::translation_reporters(id))?.all().await?.results()?;
    let votes: Vec<Value> = ctx.q(q::translation_votes(id))?.all().await?.results()?;
    json(
        200,
        &json!({ "translation": t, "reporters": reporters, "votes": votes }),
    )
}

async fn set_visibility(ctx: &Ctx, id: &str, hide: bool, reason: Option<&str>) -> HResult {
    let (text_key, lang, _) = db::translation_target(ctx, id)
        .await?
        .ok_or_else(|| api_err(404, "unknown_translation"))?;
    if hide {
        ctx.q(q::hide_translation(
            id,
            &format!("admin: {}", reason.unwrap_or("hidden")),
        ))?
        .run()
        .await?;
    } else {
        ctx.db.batch(ctx.qs(q::unhide_translation(id, &ctx.now()))?).await?;
    }
    db::recompute_current(ctx, &[(text_key, lang)]).await?;
    let (_, _, review) = db::translation_target(ctx, id)
        .await?
        .ok_or_else(|| api_err(404, "unknown_translation"))?;
    json(200, &json!({ "translation_id": id, "review": review }))
}

/// Body: one of `{"ip_hash": "..."}`, `{"net_hash": "..."}`, `{"ip": "203.0.113.7"}`
/// (exact address) or `{"range": "203.0.113.0/24"}` (its /24 or IPv6 /48),
/// plus optional `"reason"` and `"purge": true` (drop that IP hash's votes).
/// Raw addresses are hashed here and never stored.
async fn ban(ctx: &Ctx, b: &Value) -> HResult {
    let salt = ctx
        .secret("IP_SALT")
        .ok_or_else(|| api_err(503, "server_not_configured"))?;
    let get = |k: &str| {
        b.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let (hash, kind) = if let Some(h) = get("ip_hash") {
        (h.to_owned(), "ip")
    } else if let Some(h) = get("net_hash") {
        (h.to_owned(), "net")
    } else if let Some(ip) = get("ip") {
        let addr: std::net::IpAddr = ip.parse().map_err(|_| api_err(400, "bad_ip"))?;
        (hash::ip_hashes(&salt, &addr.to_string()).ip, "ip")
    } else if let Some(range) = get("range") {
        let addr: std::net::IpAddr = range
            .split('/')
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|_| api_err(400, "bad_range"))?;
        (hash::ip_hashes(&salt, &addr.to_string()).net, "net")
    } else {
        return Err(api_err(400, "missing_target"));
    };
    if !is_key(&hash) {
        return Err(api_err(400, "bad_hash"));
    }
    let reason: String = get("reason").unwrap_or("").chars().take(500).collect();
    ctx.q(q::ban_upsert(&hash, kind, &reason, &ctx.now()))?.run().await?;
    let mut purged = 0usize;
    if kind == "ip" && b.get("purge").and_then(Value::as_bool) == Some(true) {
        #[derive(Deserialize)]
        struct Row {
            translation_id: String,
            text_key: String,
            lang: String,
        }
        let rows: Vec<Row> = ctx.q(q::votes_by_ip(&hash))?.all().await?.results()?;
        purged = rows.len();
        ctx.q(q::delete_votes_by_ip(&hash))?.run().await?;
        // Pending site corrections sent only by this IP are hidden too.
        ctx.q(q::hide_corrections_only_by(&hash))?.run().await?;
        let ids: Vec<String> = rows.iter().map(|r| r.translation_id.clone()).collect();
        for chunk in ids.chunks(500) {
            db::recount_votes(ctx, chunk).await?;
        }
        let mut keys: Vec<(String, String)> = rows.into_iter().map(|r| (r.text_key, r.lang)).collect();
        keys.sort();
        keys.dedup();
        for chunk in keys.chunks(200) {
            db::recompute_current(ctx, chunk).await?;
        }
    }
    json(200, &json!({ "banned": hash, "kind": kind, "votes_purged": purged }))
}

async fn releases(ctx: &Ctx, lang: &str) -> HResult {
    let rows: Vec<Value> = ctx.q(q::release_list(lang))?.all().await?.results()?;
    json(200, &json!({ "lang": lang, "releases": rows }))
}

/// Makes `version` the `latest` release again (files are never deleted).
async fn rollback(ctx: &Ctx, lang: &str, version: Option<i64>) -> HResult {
    let version = version.ok_or_else(|| api_err(400, "missing_version"))?;
    let exists: Option<Value> = ctx.q(q::release_exists(lang, version))?.first(None).await?;
    if exists.is_none() {
        return Err(api_err(404, "unknown_version"));
    }
    ctx.q(q::release_activate(lang, version))?.run().await?;
    // Scheduled builds stop until an admin builds again (otherwise the next
    // cron run would publish a new version on top of the rollback).
    jobs::set_pin(ctx, lang, Some(version)).await?;
    json(200, &json!({ "lang": lang, "active": version, "pinned": true }))
}
