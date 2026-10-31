//! D1 access shared by handlers and jobs. All SQL comes from
//! [`crate::queries`] (sea-query, values always bound).

use serde::Deserialize;
use serde_json::json;
use worker::*;

use super::{Ctx, Fail, api_err};
use crate::api::Prepared;
use crate::hash::IpHashes;
use crate::queries as q;
use crate::ratelimit::{self, Bucket, Limit};
use crate::score::{self, Candidate};

/// Whether the caller's IP hash or network hash is banned.
pub async fn banned(ctx: &Ctx, who: &IpHashes) -> Result<bool> {
    #[derive(Deserialize)]
    struct Row {
        #[allow(dead_code)]
        x: i64,
    }
    let row: Option<Row> = ctx.q(q::ban_check(&who.ip, &who.net))?.first(None).await?;
    Ok(row.is_some())
}

/// Refuses banned callers with 403.
pub async fn ensure_not_banned(ctx: &Ctx, who: &IpHashes) -> std::result::Result<(), Fail> {
    if banned(ctx, who).await? {
        return Err(api_err(403, "banned"));
    }
    Ok(())
}

/// Takes `cost` tokens from the caller's bucket for `limit`.
pub async fn rate(ctx: &Ctx, limit: &Limit, ip_hash: &str, cost: f64) -> Result<ratelimit::Decision> {
    #[derive(Deserialize)]
    struct Row {
        tokens: f64,
        updated_ms: f64,
    }
    let key = format!("{}:{}", limit.name, ip_hash);
    let row: Option<Row> = ctx.q(q::rate_get(&key))?.first(None).await?;
    let d = ratelimit::take(
        limit,
        row.map(|r| Bucket {
            tokens: r.tokens,
            updated_ms: r.updated_ms as i64,
        }),
        ctx.now_ms,
        cost,
    );
    ctx.q(q::rate_put(&key, d.bucket.tokens, d.bucket.updated_ms))?
        .run()
        .await?;
    Ok(d)
}

/// 429 with `Retry-After` when over the limit.
pub async fn ensure_rate(ctx: &Ctx, limit: &Limit, ip_hash: &str, cost: f64) -> std::result::Result<(), Fail> {
    let d = rate(ctx, limit, ip_hash, cost).await?;
    if d.allowed {
        return Ok(());
    }
    let mut r = Response::from_json(&json!({"error": "rate_limited", "retry_after": d.retry_after}))?.with_status(429);
    r.headers_mut().set("retry-after", &d.retry_after.to_string())?;
    super::apply_headers(&mut r, crate::headers::Kind::Json)?;
    Err(Fail::Response(r))
}

/// Stores a checked contribution: texts, ID pairs, translations, distinct
/// reporters, and review queue entries. A constant number of statements per
/// request (rows go in as one JSON array each, expanded with `json_each`).
pub async fn store_contribution(ctx: &Ctx, p: &Prepared, ip_hash: &str) -> Result<()> {
    let mut texts = Vec::new();
    let mut pairs = Vec::new();
    let mut trs = Vec::new();
    for it in &p.items {
        texts.push(json!({"k": it.text_key, "en": it.en}));
        if let Some(g) = it.game_id {
            pairs.push(json!({"g": g, "k": it.text_key}));
        }
        if let Some(t) = &it.tr {
            trs.push(json!({"id": t.id, "k": it.text_key, "tr": t.tr, "src": t.source}));
        }
    }
    let (texts, pairs, trs) = (
        json!(texts).to_string(),
        json!(pairs).to_string(),
        json!(trs).to_string(),
    );
    let now = ctx.now();
    let rows = q::ContributionRows {
        texts: &texts,
        pairs: &pairs,
        trs: &trs,
        ip_hash,
        lang: &p.lang,
        now: &now,
        game_build: p.game_build.map(|b| b.min(i64::MAX as u64) as i64),
    };
    for r in ctx.db.batch(ctx.qs(q::contribution(&rows))?).await? {
        if let Some(e) = r.error() {
            return Err(Error::RustError(format!("store_contribution: {e}")));
        }
    }
    Ok(())
}

/// Recounts votes of the given translations and refreshes their score.
pub async fn recount_votes(ctx: &Ctx, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    #[derive(Deserialize)]
    struct Row {
        id: String,
        up: i64,
        down: i64,
    }
    let list = json!(ids).to_string();
    ctx.q(q::recount_votes(&list))?.run().await?;
    let rows: Vec<Row> = ctx.q(q::translations_votes(&list))?.all().await?.results()?;
    let scores = ctx.qs(rows
        .iter()
        .map(|r| q::set_score(&r.id, score::wilson_lower_bound(r.up, r.down)))
        .collect())?;
    if !scores.is_empty() {
        ctx.db.batch(scores).await?;
    }
    Ok(())
}

/// Recomputes the current translation of each `(text_key, lang)`.
pub async fn recompute_current(ctx: &Ctx, keys: &[(String, String)]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    #[derive(Deserialize)]
    struct Row {
        id: String,
        text_key: String,
        lang: String,
        review: String,
        up: i64,
        down: i64,
        passed_at: Option<String>,
    }
    let list = json!(keys.iter().map(|(k, l)| json!({"k": k, "l": l})).collect::<Vec<_>>()).to_string();
    let rows: Vec<Row> = ctx.q(q::current_candidates(&list))?.all().await?.results()?;
    let now = ctx.now();
    let mut stmts = Vec::new();
    for (k, l) in keys {
        let cands: Vec<Candidate> = rows
            .iter()
            .filter(|r| &r.text_key == k && &r.lang == l)
            .map(|r| Candidate {
                id: r.id.clone(),
                review: r.review.clone(),
                up: r.up,
                down: r.down,
                passed_at: r.passed_at.clone().unwrap_or_default(),
            })
            .collect();
        stmts.push(match score::pick_current(&cands) {
            Some(c) => q::current_upsert(&q::CurrentRow {
                text_key: k,
                lang: l,
                translation_id: &c.id,
                score: score::wilson_lower_bound(c.up, c.down),
                up: c.up,
                down: c.down,
                now: &now,
            }),
            None => q::current_delete(k, l),
        });
    }
    ctx.db.batch(ctx.qs(stmts)?).await?;
    Ok(())
}

/// Text key, language and review state of a translation.
pub async fn translation_target(ctx: &Ctx, id: &str) -> Result<Option<(String, String, String)>> {
    #[derive(Deserialize)]
    struct Row {
        text_key: String,
        lang: String,
        review: String,
    }
    let r: Option<Row> = ctx.q(q::translation_target(id))?.first(None).await?;
    Ok(r.map(|r| (r.text_key, r.lang, r.review)))
}

pub async fn counter(ctx: &Ctx, name: &str) -> Result<i64> {
    #[derive(Deserialize)]
    struct Row {
        value: i64,
    }
    let r: Option<Row> = ctx.q(q::counter_get(name))?.first(None).await?;
    Ok(r.map(|r| r.value).unwrap_or(0))
}

/// A cached `kv` value.
pub async fn kv_get(ctx: &Ctx, key: &str) -> Result<Option<String>> {
    #[derive(Deserialize)]
    struct Row {
        value: String,
    }
    let r: Option<Row> = ctx.q(q::kv_get(key))?.first(None).await?;
    Ok(r.map(|r| r.value))
}
