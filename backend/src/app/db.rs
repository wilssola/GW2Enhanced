//! D1 access shared by handlers and jobs. All SQL comes from
//! [`crate::queries`] (sea-query, values always bound).

use serde::Deserialize;
use serde_json::json;
use worker::*;

use super::{Ctx, Fail, api_err};
use crate::api::Prepared;
use crate::budget::{self, Tier};
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

thread_local! {
    static MEMORY_BUCKETS: std::cell::RefCell<ratelimit::MemoryBuckets> = Default::default();
    static REFUSALS: std::cell::RefCell<ratelimit::Refusals> = Default::default();
    static WRITE_GATE: std::cell::RefCell<budget::WriteGate> = Default::default();
    static READ_METER: std::cell::RefCell<budget::ReadMeter> = Default::default();
}

/// Buckets that allowed a request, to store once the request is going to write anyway (after its Turnstile check and
/// budget reservation), so a refused or failing request writes nothing.
pub struct RateCheck {
    puts: Vec<(String, Bucket)>,
}

impl RateCheck {
    pub fn statements(&self, ctx: &Ctx) -> Result<Vec<D1PreparedStatement>> {
        ctx.qs(self.puts.iter().map(|(k, b)| q::rate_put(k, b.tokens, b.updated_ms)).collect())
    }
}

/// Takes `cost` tokens from each `(limit, hash)` bucket (one D1 read for all, none while a refusal is remembered).
/// `Err(retry_after)` when any of them is short; nothing is stored either way (see [`RateCheck`]).
pub async fn rate_check(
    ctx: &Ctx,
    checks: &[(&Limit, &str)],
    cost: f64,
) -> Result<std::result::Result<RateCheck, u64>> {
    let keys: Vec<String> = checks.iter().map(|(l, h)| format!("{}:{}", l.name, h)).collect();
    let remembered = REFUSALS.with(|r| {
        let r = r.borrow();
        keys.iter().filter_map(|k| r.retry_after(k, ctx.now_ms)).max()
    });
    if let Some(s) = remembered {
        return Ok(Err(s));
    }
    #[derive(Deserialize)]
    struct Row {
        bucket: String,
        tokens: f64,
        updated_ms: f64,
    }
    let rows: Vec<Row> = ctx.all(ctx.q(q::rate_get_many(&keys))?).await?.results()?;
    let mut puts = Vec::new();
    let mut refused: Option<u64> = None;
    for ((limit, _), key) in checks.iter().zip(&keys) {
        let state = rows.iter().find(|r| &r.bucket == key).map(|r| Bucket {
            tokens: r.tokens,
            updated_ms: r.updated_ms as i64,
        });
        let d = ratelimit::take(limit, state, ctx.now_ms, cost);
        if d.allowed {
            puts.push((key.clone(), d.bucket));
        } else {
            REFUSALS.with(|r| r.borrow_mut().remember(key, ctx.now_ms, d.retry_after));
            refused = Some(refused.unwrap_or(0).max(d.retry_after));
        }
    }
    Ok(match refused {
        Some(s) => Err(s),
        None => Ok(RateCheck { puts }),
    })
}

/// [`rate_check`], answering 429 with `Retry-After` when over a limit.
pub async fn ensure_rate(ctx: &Ctx, checks: &[(&Limit, &str)], cost: f64) -> std::result::Result<RateCheck, Fail> {
    match rate_check(ctx, checks, cost).await? {
        Ok(c) => Ok(c),
        Err(s) => Err(denied(429, "rate_limited", s)?),
    }
}

/// The read limit of public lists, kept in the isolate's memory only: a read never writes to D1. It is per isolate,
/// so it only slows one client down; the daily read budget is the hard ceiling.
pub fn ensure_read_rate(ctx: &Ctx, limit: &Limit, ip_hash: &str, cost: f64) -> std::result::Result<(), Fail> {
    let key = format!("{}:{}", limit.name, ip_hash);
    let d = MEMORY_BUCKETS.with(|m| m.borrow_mut().take(&key, limit, ctx.now_ms, cost));
    if d.allowed { Ok(()) } else { Err(denied(429, "rate_limited", d.retry_after)?) }
}

/// A JSON error with `Retry-After`.
fn denied(status: u16, code: &str, retry_after: u64) -> std::result::Result<Fail, Fail> {
    let mut r = Response::from_json(&json!({"error": code, "retry_after": retry_after}))?.with_status(status);
    r.headers_mut().set("retry-after", &retry_after.to_string())?;
    r.headers_mut().set("cache-control", "no-store")?;
    super::apply_headers(&mut r, crate::headers::Kind::Json)?;
    Ok(Fail::Response(r))
}

// ------------------------------------------------------------ daily budgets ---

fn write_budget(ctx: &Ctx) -> u64 {
    budget::parse_budget(&ctx.var("WRITE_BUDGET_DAILY", ""), budget::WRITE_BUDGET_DAILY)
}

fn read_budget(ctx: &Ctx) -> u64 {
    budget::parse_budget(&ctx.var("READ_BUDGET_DAILY", ""), budget::READ_BUDGET_DAILY)
}

#[derive(Deserialize)]
struct CounterRow {
    value: i64,
}

/// Reserves `units` of today's write budget (one atomic upsert; nothing written when refused, and a refusal is
/// remembered by the isolate so the next ones do not even ask D1).
pub async fn reserve_writes(ctx: &Ctx, units: u64, tier: Tier) -> Result<bool> {
    let day = budget::day(ctx.now_ms);
    let cap = budget::cap(write_budget(ctx), tier);
    if units > cap || WRITE_GATE.with(|g| g.borrow().refuses(&day, units, cap)) {
        return Ok(false);
    }
    let name = budget::write_counter(ctx.now_ms);
    let rows: Vec<CounterRow> = ctx
        .all(ctx.q(q::budget_reserve(&name, units as i64, cap as i64))?)
        .await?
        .results()?;
    Ok(match rows.first() {
        Some(r) => {
            WRITE_GATE.with(|g| g.borrow_mut().accepted(&day, r.value.max(0) as u64));
            true
        }
        None => {
            WRITE_GATE.with(|g| g.borrow_mut().refused(&day, units));
            false
        }
    })
}

/// [`reserve_writes`] for a public request: 503 `write_budget_exhausted` until midnight UTC when refused.
pub async fn ensure_writes(ctx: &Ctx, units: u64) -> std::result::Result<(), Fail> {
    if reserve_writes(ctx, units, Tier::Public).await? {
        Ok(())
    } else {
        Err(denied(503, "write_budget_exhausted", budget::seconds_to_midnight(ctx.now_ms))?)
    }
}

/// Adds `delta` rows (negative: refunds an over-estimate) to today's write budget.
pub async fn charge_writes(ctx: &Ctx, delta: i64) -> Result<()> {
    if delta == 0 {
        return Ok(());
    }
    let name = budget::write_counter(ctx.now_ms);
    let rows: Vec<CounterRow> = ctx.all(ctx.q(q::budget_add(&name, delta))?).await?.results()?;
    if let Some(r) = rows.first() {
        let day = budget::day(ctx.now_ms);
        WRITE_GATE.with(|g| g.borrow_mut().accepted(&day, r.value.max(0) as u64));
    }
    Ok(())
}

/// Whether today's write budget still has room for `tier` (one row read).
pub async fn writes_left(ctx: &Ctx, tier: Tier) -> Result<bool> {
    let used = counter(ctx, &budget::write_counter(ctx.now_ms)).await?.max(0) as u64;
    Ok(used < budget::cap(write_budget(ctx), tier))
}

/// Whether today's read budget allows another list query for `tier` (D1 is asked at most once per flush interval).
pub async fn reads_allowed(ctx: &Ctx, tier: Tier) -> Result<bool> {
    let day = budget::day(ctx.now_ms);
    let cap = budget::cap(read_budget(ctx), tier);
    if let Some(ok) = READ_METER.with(|m| m.borrow_mut().allows(&day, cap)) {
        return Ok(ok);
    }
    let total = counter(ctx, &budget::read_counter(ctx.now_ms)).await?.max(0) as u64;
    Ok(READ_METER.with(|m| {
        let mut m = m.borrow_mut();
        m.synced(&day, total, ctx.now_ms);
        m.allows(&day, cap).unwrap_or(true)
    }))
}

/// Counts `rows` read by a list query; every [`budget::ReadMeter::FLUSH_ROWS`] rows or 30 s the isolate adds its
/// rows to today's counter (one write). Best effort: a failed flush is retried by the next request.
pub async fn charge_reads(ctx: &Ctx, rows: u64) {
    let day = budget::day(ctx.now_ms);
    let send = READ_METER.with(|m| {
        let mut m = m.borrow_mut();
        m.add(&day, rows);
        m.should_flush(ctx.now_ms).then(|| m.pending())
    });
    let Some(send) = send else { return };
    let name = budget::read_counter(ctx.now_ms);
    let Ok(stmt) = ctx.q(q::budget_add(&name, send as i64)) else { return };
    if let Ok(r) = stmt.all().await
        && let Ok(rows) = r.results::<CounterRow>()
        && let Some(v) = rows.first()
    {
        READ_METER.with(|m| m.borrow_mut().flushed(&day, send, v.value.max(0) as u64, ctx.now_ms));
    }
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
    for r in ctx.batch(ctx.qs(q::contribution(&rows))?).await? {
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
    ctx.run(ctx.q(q::recount_votes(&list))?).await?;
    let rows: Vec<Row> = ctx.q(q::translations_votes(&list))?.all().await?.results()?;
    let scores = ctx.qs(rows
        .iter()
        .map(|r| q::set_score(&r.id, score::wilson_lower_bound(r.up, r.down)))
        .collect())?;
    if !scores.is_empty() {
        ctx.batch(scores).await?;
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
    ctx.batch(ctx.qs(stmts)?).await?;
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
