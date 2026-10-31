//! Scheduled jobs: review queue, stats, release builds, D1 → R2 backups.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};
use worker::*;

use super::Ctx;
use super::db;
use super::external::ai_review;
use crate::chunk::gzip;
use crate::manifest;
use crate::queries::{self as q, BackupTable};
use crate::release::{self, Builder, DictRecord, parse_ids, release_src};
use crate::review::{self, Task, Verdict};
use crate::score;
use crate::trust::{self, PairCandidate, State};
use crate::validate;

/// Items leased from the queue per run (`REVIEW_BATCH` var).
const DEFAULT_REVIEW_BATCH: usize = 200;
/// A leased item that was not finished is retried after this.
const LEASE_MS: i64 = 15 * 60 * 1000;
/// A translation waiting for its text's review is retried after this.
const WAIT_TEXT_MS: i64 = 10 * 60 * 1000;
/// Attempts before an item is dropped from the queue.
const MAX_ATTEMPTS: i64 = 12;

#[derive(Debug, Default, serde::Serialize)]
pub struct ReviewReport {
    pub leased: usize,
    pub texts_trusted: usize,
    pub texts_rejected: usize,
    pub pairs_decided: usize,
    pub passed: usize,
    pub flagged: usize,
    pub deferred: usize,
    pub ai_calls: usize,
}

#[derive(Deserialize)]
struct QueueRow {
    id: i64,
    kind: String,
    #[serde(rename = "ref")]
    reference: String,
    attempts: i64,
}

fn list(v: &[impl serde::Serialize]) -> String {
    json!(v).to_string()
}

/// Drains a batch of the review queue: rule checks, reporter agreement and
/// the AI review (NVIDIA NIM), then refreshes stats.
pub async fn review(ctx: &Ctx) -> Result<ReviewReport> {
    let mut rep = ReviewReport::default();
    let batch: usize = ctx
        .var("REVIEW_BATCH", "")
        .parse()
        .unwrap_or(DEFAULT_REVIEW_BATCH)
        .clamp(1, 500);
    let rows: Vec<QueueRow> = ctx.q(q::queue_due(ctx.now_ms, batch as u64))?.all().await?.results()?;
    rep.leased = rows.len();
    if !rows.is_empty() {
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        ctx.q(q::queue_lease(&list(&ids), ctx.now_ms + LEASE_MS))?.run().await?;
        process(ctx, &rows, &mut rep).await?;
    }
    // Housekeeping: full buckets need no row.
    ctx.q(q::rate_cleanup(ctx.now_ms - 2 * 86_400_000))?.run().await?;
    for lang in ctx.langs().split(',').map(str::trim).filter(|l| !l.is_empty()) {
        let v = compute_stats(ctx, lang).await?;
        ctx.q(q::kv_put(&format!("stats:{lang}"), &v.to_string(), &ctx.now()))?
            .run()
            .await?;
    }
    Ok(rep)
}

/// What to do with a leased queue item.
enum Outcome {
    Done,
    Retry(i64),
    Leave,
}

async fn process(ctx: &Ctx, rows: &[QueueRow], rep: &mut ReviewReport) -> Result<()> {
    let now = ctx.now();
    let mut outcome: HashMap<i64, Outcome> = HashMap::new();
    let mut writes: Vec<D1PreparedStatement> = Vec::new();
    // Tasks for the AI, with what to do on a verdict.
    enum Target {
        Text(String),
        Translation { id: String, text_key: String, lang: String },
    }
    let mut ai: Vec<(i64, Task, Target)> = Vec::new();

    // --- English texts ---
    #[derive(Deserialize)]
    struct TextRow {
        key: String,
        en: String,
        state: String,
        reporters: i64,
    }
    let text_keys: Vec<&str> = rows
        .iter()
        .filter(|r| r.kind == "text")
        .map(|r| r.reference.as_str())
        .collect();
    let texts: Vec<TextRow> = if text_keys.is_empty() {
        Vec::new()
    } else {
        ctx.q(q::texts_by_keys(&list(&text_keys)))?.all().await?.results()?
    };
    let mut trusted_texts: Vec<String> = Vec::new();
    for r in rows.iter().filter(|r| r.kind == "text") {
        let Some(t) = texts.iter().find(|t| t.key == r.reference) else {
            outcome.insert(r.id, Outcome::Done);
            continue;
        };
        if t.state != "pending" {
            outcome.insert(r.id, Outcome::Done);
        } else if let Err(e) = validate::check_en(&t.en) {
            writes.push(set_text(ctx, &t.key, State::Rejected, e.as_str(), &now)?);
            rep.texts_rejected += 1;
            outcome.insert(r.id, Outcome::Done);
        } else if trust::text_trusted_by_reporters(t.reporters) {
            writes.push(set_text(ctx, &t.key, State::Trusted, "reporters", &now)?);
            trusted_texts.push(t.key.clone());
            rep.texts_trusted += 1;
            outcome.insert(r.id, Outcome::Done);
        } else if r.attempts >= MAX_ATTEMPTS {
            outcome.insert(r.id, Outcome::Done);
        } else {
            ai.push((r.id, Task::Text { en: t.en.clone() }, Target::Text(t.key.clone())));
        }
    }

    // --- Translations ---
    #[derive(Deserialize)]
    struct TrRow {
        id: String,
        text_key: String,
        lang: String,
        tr: String,
        review: String,
        en: String,
        text_state: String,
    }
    let tr_ids: Vec<&str> = rows
        .iter()
        .filter(|r| r.kind == "translation")
        .map(|r| r.reference.as_str())
        .collect();
    let trs: Vec<TrRow> = if tr_ids.is_empty() {
        Vec::new()
    } else {
        ctx.q(q::translations_for_review(&list(&tr_ids)))?
            .all()
            .await?
            .results()?
    };
    for r in rows.iter().filter(|r| r.kind == "translation") {
        let Some(t) = trs.iter().find(|t| t.id == r.reference) else {
            outcome.insert(r.id, Outcome::Done);
            continue;
        };
        if t.review != "pending" {
            outcome.insert(r.id, Outcome::Done);
        } else if t.text_state == "rejected" {
            writes.push(set_review(ctx, &t.id, "flagged", "text_rejected", &now)?);
            rep.flagged += 1;
            outcome.insert(r.id, Outcome::Done);
        } else if let Err(e) = validate::check_tr(&t.en, &t.tr) {
            writes.push(set_review(ctx, &t.id, "flagged", e.as_str(), &now)?);
            rep.flagged += 1;
            outcome.insert(r.id, Outcome::Done);
        } else if r.attempts >= MAX_ATTEMPTS {
            outcome.insert(r.id, Outcome::Done);
        } else if t.text_state == "pending" && !trusted_texts.contains(&t.text_key) {
            rep.deferred += 1;
            outcome.insert(r.id, Outcome::Retry(WAIT_TEXT_MS));
        } else {
            ai.push((
                r.id,
                Task::Translation {
                    en: t.en.clone(),
                    tr: t.tr.clone(),
                    lang: t.lang.clone(),
                },
                Target::Translation {
                    id: t.id.clone(),
                    text_key: t.text_key.clone(),
                    lang: t.lang.clone(),
                },
            ));
        }
    }

    // --- AI review, in batches ---
    let mut passed_keys: Vec<(String, String)> = Vec::new();
    for group in ai.chunks(review::BATCH) {
        let tasks: Vec<Task> = group.iter().map(|(_, t, _)| t.clone()).collect();
        let verdicts = match ai_review(ctx, &tasks).await {
            Ok(Some(v)) => {
                rep.ai_calls += 1;
                v
            }
            // No API key: leave everything leased, it is retried later.
            Ok(None) => break,
            Err(e) => {
                console_warn!("AI review call failed: {e}");
                vec![None; tasks.len()]
            }
        };
        for ((qid, _, target), verdict) in group.iter().zip(verdicts) {
            let Some(Verdict { ok, reason }) = verdict else {
                outcome.insert(*qid, Outcome::Leave);
                continue;
            };
            let note = format!("ai: {reason}");
            match target {
                Target::Text(key) => {
                    let state = if ok { State::Trusted } else { State::Rejected };
                    writes.push(set_text(ctx, key, state, &note, &now)?);
                    if ok {
                        trusted_texts.push(key.clone());
                        rep.texts_trusted += 1;
                    } else {
                        rep.texts_rejected += 1;
                    }
                }
                Target::Translation { id, text_key, lang } => {
                    writes.push(set_review(ctx, id, if ok { "passed" } else { "flagged" }, &note, &now)?);
                    if ok {
                        rep.passed += 1;
                        passed_keys.push((text_key.clone(), lang.clone()));
                    } else {
                        rep.flagged += 1;
                    }
                }
            }
            outcome.insert(*qid, Outcome::Done);
        }
    }
    if !trusted_texts.is_empty() {
        // Pairs of newly trusted texts may now be decidable.
        writes.push(ctx.q(q::enqueue_pairs_of_texts(&list(&trusted_texts), &now))?);
    }
    if !writes.is_empty() {
        ctx.db.batch(std::mem::take(&mut writes)).await?;
    }

    // --- ID ↔ text pairs (after text states are written) ---
    let pair_rows: Vec<&QueueRow> = rows.iter().filter(|r| r.kind == "pair").collect();
    let game_ids: Vec<i64> = pair_rows
        .iter()
        .filter_map(|r| r.reference.split(':').next()?.parse().ok())
        .collect();
    if !game_ids.is_empty() {
        #[derive(Deserialize)]
        struct PairRow {
            game_id: i64,
            text_key: String,
            reporters: i64,
            state: String,
            text_state: String,
        }
        let pairs: Vec<PairRow> = ctx.q(q::pairs_for_ids(&list(&game_ids)))?.all().await?.results()?;
        let mut done_ids = std::collections::HashSet::new();
        for g in &game_ids {
            if !done_ids.insert(*g) {
                continue;
            }
            let cands: Vec<PairCandidate> = pairs
                .iter()
                .filter(|p| p.game_id == *g)
                .map(|p| PairCandidate {
                    text_key: p.text_key.clone(),
                    reporters: p.reporters,
                    state: State::parse(&p.state),
                    text_state: State::parse(&p.text_state),
                })
                .collect();
            for ((key, new), old) in trust::decide_pairs(&cands).into_iter().zip(&cands) {
                if new != old.state {
                    rep.pairs_decided += 1;
                    writes.push(ctx.q(q::set_pair_state(*g, &key, new.as_str(), &now))?);
                }
            }
        }
        // Undecided pairs leave the queue: a new report or their text being
        // trusted queues them again.
        for r in pair_rows {
            outcome.insert(r.id, Outcome::Done);
        }
    }

    // --- Queue bookkeeping ---
    let done: Vec<i64> = outcome
        .iter()
        .filter(|(_, o)| matches!(o, Outcome::Done))
        .map(|(id, _)| *id)
        .collect();
    if !done.is_empty() {
        writes.push(ctx.q(q::queue_delete(&list(&done)))?);
    }
    for (id, o) in &outcome {
        if let Outcome::Retry(ms) = o {
            writes.push(ctx.q(q::queue_retry(*id, ctx.now_ms + ms))?);
        }
    }
    if !writes.is_empty() {
        ctx.db.batch(writes).await?;
    }
    passed_keys.sort();
    passed_keys.dedup();
    db::recompute_current(ctx, &passed_keys).await?;
    Ok(())
}

fn set_text(ctx: &Ctx, key: &str, state: State, note: &str, now: &str) -> Result<D1PreparedStatement> {
    ctx.q(q::set_text_state(key, state.as_str(), note, now))
}

fn set_review(ctx: &Ctx, id: &str, review: &str, note: &str, now: &str) -> Result<D1PreparedStatement> {
    ctx.q(q::set_review(id, review, note, now))
}

/// Site metrics for a language.
pub async fn compute_stats(ctx: &Ctx, lang: &str) -> Result<Value> {
    #[derive(Deserialize)]
    struct Row {
        texts: i64,
        translated: i64,
        perfect: i64,
        pending: i64,
    }
    let r: Option<Row> = ctx
        .q(q::stats(lang, score::PERFECT_MIN_VOTES, score::PERFECT_MIN_SCORE))?
        .first(None)
        .await?;
    let r = r.unwrap_or(Row {
        texts: 0,
        translated: 0,
        perfect: 0,
        pending: 0,
    });
    let pct = |x: i64| {
        if r.texts == 0 {
            0.0
        } else {
            (x as f64 * 10_000.0 / r.texts as f64).round() / 100.0
        }
    };
    Ok(json!({
        "lang": lang,
        "texts": r.texts,
        "translated": r.translated,
        "perfect": r.perfect,
        "translated_pct": pct(r.translated),
        "perfect_pct": pct(r.perfect),
        "review_queue": r.pending,
        "updated_at": ctx.now(),
    }))
}

pub async fn cached_stats(ctx: &Ctx, lang: &str) -> Result<Option<Value>> {
    Ok(db::kv_get(ctx, &format!("stats:{lang}"))
        .await?
        .and_then(|v| serde_json::from_str(&v).ok()))
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildResult {
    Built {
        lang: String,
        version: u64,
        records: usize,
        files: usize,
    },
    Unchanged {
        lang: String,
        version: u64,
    },
    /// An admin rolled back: scheduled builds wait until an admin builds again.
    Pinned {
        lang: String,
        version: i64,
    },
}

fn pin_key(lang: &str) -> String {
    format!("release_pin:{lang}")
}

/// Version pinned by a rollback, if any.
pub async fn pinned(ctx: &Ctx, lang: &str) -> Result<Option<i64>> {
    Ok(db::kv_get(ctx, &pin_key(lang)).await?.and_then(|v| v.parse().ok()))
}

pub async fn set_pin(ctx: &Ctx, lang: &str, version: Option<i64>) -> Result<()> {
    let stmt = match version {
        Some(v) => q::kv_put(&pin_key(lang), &v.to_string(), &ctx.now()),
        None => q::kv_delete(&pin_key(lang)),
    };
    ctx.q(stmt)?.run().await?;
    Ok(())
}

/// Scheduled build of every language (skips pinned ones).
pub async fn build_all_releases(ctx: &Ctx, force: bool) -> Result<Vec<BuildResult>> {
    let mut out = Vec::new();
    for lang in ctx.langs().split(',').map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(version) = pinned(ctx, lang).await? {
            out.push(BuildResult::Pinned {
                lang: lang.into(),
                version,
            });
            continue;
        }
        out.push(build_release(ctx, lang, force).await?);
    }
    Ok(out)
}

const RELEASE_PAGE: u64 = 5000;

/// Builds `releases/{lang}/{version}/dictionary-NNN.jsonl.gz` from the current
/// translations, signs the manifest and makes it `latest`. Skipped when the
/// content equals the active release (unless `force`).
pub async fn build_release(ctx: &Ctx, lang: &str, force: bool) -> Result<BuildResult> {
    let key_b64 = ctx
        .secret("RELEASE_SIGNING_KEY")
        .ok_or_else(|| Error::RustError("RELEASE_SIGNING_KEY not set".into()))?;
    let key = manifest::signing_key(&key_b64).map_err(Error::RustError)?;
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
        let rows: Vec<Row> = ctx
            .q(q::release_page(lang, &after, RELEASE_PAGE))?
            .all()
            .await?
            .results()?;
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
    let active: Option<Active> = ctx.q(q::active_release(lang))?.first(None).await?;
    if let Some(a) = &active
        && a.content_hash == content_hash
        && !force
    {
        return Ok(BuildResult::Unchanged {
            lang: lang.into(),
            version: a.version as u64,
        });
    }
    #[derive(Deserialize)]
    struct Max {
        v: Option<i64>,
    }
    let max: Option<Max> = ctx.q(q::max_version(lang))?.first(None).await?;
    let version = max.and_then(|m| m.v).unwrap_or(0) as u64 + 1;
    let created_at = ctx.now();
    let m = release::signed_manifest(lang, version, &created_at, &files, &key);
    let manifest_json = serde_json::to_string(&m)?;

    let bucket = ctx.bucket()?;
    for c in &files {
        bucket
            .put(release::r2_key(lang, version, &c.entry.name), c.bytes.clone())
            .http_metadata(HttpMetadata {
                content_type: Some("application/gzip".into()),
                cache_control: Some("public, max-age=31536000, immutable".into()),
                ..Default::default()
            })
            .execute()
            .await?;
    }
    bucket
        .put(release::r2_key(lang, version, "manifest.json"), manifest_json.clone())
        .http_metadata(HttpMetadata {
            content_type: Some("application/json".into()),
            ..Default::default()
        })
        .execute()
        .await?;
    let files_json = serde_json::to_string(&m.files)?;
    let new = q::NewRelease {
        lang,
        version: version as i64,
        created_at: &created_at,
        files: &files_json,
        manifest: &manifest_json,
        content_hash: &content_hash,
        records: records as i64,
    };
    ctx.db.batch(ctx.qs(q::release_insert(&new))?).await?;
    Ok(BuildResult::Built {
        lang: lang.into(),
        version,
        records,
        files: files.len(),
    })
}

const BACKUP_PAGE: u64 = 5000;
const BACKUP_PART_ROWS: usize = 50_000;

#[derive(Debug, serde::Serialize)]
pub struct BackupReport {
    pub prefix: String,
    pub rows: usize,
    pub files: usize,
}

/// Exports every table to `backups/{timestamp}/{table}-NNN.jsonl.gz`.
pub async fn backup(ctx: &Ctx) -> Result<BackupReport> {
    let prefix = format!("backups/{}", crate::time::compact(ctx.now_ms));
    let bucket = ctx.bucket()?;
    let mut report = BackupReport {
        prefix: prefix.clone(),
        rows: 0,
        files: 0,
    };
    for table in BackupTable::ALL {
        let name = table.name();
        let mut after = 0i64;
        let mut part = 0usize;
        let mut buf: Vec<u8> = Vec::new();
        let mut rows_in_part = 0usize;
        loop {
            let rows: Vec<Value> = ctx
                .q(q::backup_page(table, after, BACKUP_PAGE))?
                .all()
                .await?
                .results()?;
            for r in &rows {
                serde_json::to_writer(&mut buf, r)?;
                buf.push(b'\n');
            }
            rows_in_part += rows.len();
            report.rows += rows.len();
            let last = rows
                .last()
                .and_then(|r| r.get("_rowid"))
                .and_then(Value::as_f64)
                .map(|v| v as i64);
            let finished = (rows.len() as u64) < BACKUP_PAGE;
            if rows_in_part >= BACKUP_PART_ROWS || (finished && (rows_in_part > 0 || part == 0)) {
                let key = format!("{prefix}/{name}-{part:03}.jsonl.gz");
                bucket
                    .put(key, gzip(&buf))
                    .http_metadata(HttpMetadata {
                        content_type: Some("application/gzip".into()),
                        ..Default::default()
                    })
                    .execute()
                    .await?;
                report.files += 1;
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
    bucket
        .put(
            format!("{prefix}/done.json"),
            json!({ "rows": report.rows, "files": report.files, "tables": tables }).to_string(),
        )
        .execute()
        .await?;
    Ok(report)
}
