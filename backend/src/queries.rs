//! Every SQL statement the Worker runs, built with [`sea_query`] (SQLite
//! dialect). Each function returns `(sql, values)`: the SQL text contains only
//! identifiers and `?` placeholders, and every value (including all data from
//! clients) is in `values`, bound through D1's `prepare().bind()`.
//!
//! Pure Rust: the host tests run every statement against the real migration in
//! an in-memory SQLite.

use sea_query::extension::sqlite::SqliteExpr;
use sea_query::{
    Alias, Cond, Expr, ExprTrait, Func, Iden, JoinType, OnConflict, Order, Query, SelectStatement, SqliteQueryBuilder,
    Values,
};

/// A built statement: SQL with `?` placeholders and the values to bind.
pub type Q = (String, Values);

// ---------------------------------------------------------------- schema ---

#[derive(Iden, Clone, Copy)]
pub enum Texts {
    Table,
    Key,
    En,
    State,
    Reporters,
    ReviewNote,
    CreatedAt,
    ReviewedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum TextReporters {
    Table,
    TextKey,
    IpHash,
}

#[derive(Iden, Clone, Copy)]
pub enum IdPairs {
    Table,
    GameId,
    TextKey,
    State,
    Reporters,
    FirstBuild,
    LastBuild,
    CreatedAt,
    ReviewedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum IdPairReporters {
    Table,
    GameId,
    TextKey,
    IpHash,
}

#[derive(Iden, Clone, Copy)]
pub enum Translations {
    Table,
    Id,
    TextKey,
    Lang,
    Tr,
    Source,
    Review,
    ReviewNote,
    Up,
    Down,
    Score,
    Reporters,
    CreatedAt,
    PassedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum TranslationReporters {
    Table,
    TranslationId,
    IpHash,
}

#[derive(Iden, Clone, Copy)]
pub enum Current {
    Table,
    TextKey,
    Lang,
    TranslationId,
    Score,
    Up,
    Down,
    UpdatedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum Votes {
    Table,
    TranslationId,
    IpHash,
    Value,
    CreatedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum ReviewQueue {
    Table,
    Id,
    Kind,
    Ref,
    EnqueuedAt,
    Attempts,
    NotBeforeMs,
}

#[derive(Iden, Clone, Copy)]
pub enum Releases {
    Table,
    Lang,
    Version,
    CreatedAt,
    Files,
    Manifest,
    ContentHash,
    Records,
    Active,
}

#[derive(Iden, Clone, Copy)]
pub enum Counters {
    Table,
    Name,
    Value,
}

#[derive(Iden, Clone, Copy)]
pub enum RateLimits {
    Table,
    Bucket,
    Tokens,
    UpdatedMs,
}

#[derive(Iden, Clone, Copy)]
pub enum Bans {
    Table,
    Hash,
    Kind,
    Reason,
    CreatedAt,
}

#[derive(Iden, Clone, Copy)]
pub enum Kv {
    Table,
    Key,
    Value,
    UpdatedAt,
}

/// `json_each(?)` table function and its `value` column.
#[derive(Iden, Clone, Copy)]
enum JsonEach {
    #[iden = "json_each"]
    Func,
    Value,
}

#[derive(Iden, Clone, Copy)]
#[iden = "group_concat"]
struct GroupConcat;

/// Scalar `max(a, b)`.
#[derive(Iden, Clone, Copy)]
#[iden = "max"]
struct MaxScalar;

#[derive(Iden, Clone, Copy)]
#[iden = "rowid"]
struct RowId;

#[derive(Iden, Clone, Copy)]
#[iden = "excluded"]
struct Excluded;

// Table aliases.
#[derive(Iden, Clone, Copy)]
#[iden = "t"]
struct T;
#[derive(Iden, Clone, Copy)]
#[iden = "x"]
struct X;
#[derive(Iden, Clone, Copy)]
#[iden = "y"]
struct Y;
#[derive(Iden, Clone, Copy)]
#[iden = "c"]
struct C;
#[derive(Iden, Clone, Copy)]
#[iden = "p"]
struct P;
#[derive(Iden, Clone, Copy)]
#[iden = "r"]
struct R;
#[derive(Iden, Clone, Copy)]
#[iden = "v"]
struct V;
#[derive(Iden, Clone, Copy)]
#[iden = "j"]
struct J;

// --------------------------------------------------------------- helpers ---

fn sqlite<S>(s: &S) -> Q
where
    S: sea_query::QueryStatementWriter,
{
    s.build(SqliteQueryBuilder)
}

/// `j.value ->> ?field` (the field name is bound too).
fn jf(field: &str) -> Expr {
    Expr::col((J, JsonEach::Value)).cast_json_field(field)
}

/// `SELECT <exprs> FROM json_each(?json) AS j WHERE ? = ?`. The WHERE keeps
/// SQLite's upsert parser unambiguous in `INSERT ... SELECT ... ON CONFLICT`.
fn from_json(json: &str) -> SelectStatement {
    Query::select()
        .from_function(Func::cust(JsonEach::Func).arg(json), J)
        .and_where(Expr::val(1).eq(1))
        .to_owned()
}

/// `SELECT j.value ->> field FROM json_each(?)`, or `j.value` without a field.
fn json_list(json: &str, field: Option<&str>) -> SelectStatement {
    let mut s = from_json(json);
    match field {
        Some(f) => s.expr(jf(f)),
        None => s.expr(Expr::col((J, JsonEach::Value))),
    };
    s
}

/// `SELECT j.value ->> a, j.value ->> b FROM json_each(?)`.
fn json_pairs(json: &str, a: &str, b: &str) -> SelectStatement {
    from_json(json).expr(jf(a)).expr(jf(b)).to_owned()
}

fn do_nothing() -> OnConflict {
    OnConflict::new().do_nothing().to_owned()
}

fn count_where(table: impl Iden + Clone + 'static, cond: Cond) -> SelectStatement {
    Query::select()
        .expr(Func::count(Expr::val(1)))
        .from(table)
        .cond_where(cond)
        .to_owned()
}

// ------------------------------------------------------------ bans / rate ---

pub fn ban_check(ip: &str, net: &str) -> Q {
    sqlite(
        &Query::select()
            .expr_as(Expr::val(1), Alias::new("x"))
            .from(Bans::Table)
            .and_where(Expr::col(Bans::Hash).is_in([ip, net]))
            .limit(1)
            .to_owned(),
    )
}

pub fn ban_upsert(hash: &str, kind: &str, reason: &str, now: &str) -> Q {
    sqlite(
        &Query::insert()
            .into_table(Bans::Table)
            .columns([Bans::Hash, Bans::Kind, Bans::Reason, Bans::CreatedAt])
            .values_panic([hash.into(), kind.into(), reason.into(), now.into()])
            .on_conflict(OnConflict::column(Bans::Hash).update_column(Bans::Reason).to_owned())
            .to_owned(),
    )
}

pub fn ban_delete(hash: &str) -> Q {
    sqlite(
        &Query::delete()
            .from_table(Bans::Table)
            .and_where(Expr::col(Bans::Hash).eq(hash))
            .to_owned(),
    )
}

pub fn rate_get(bucket: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([RateLimits::Tokens, RateLimits::UpdatedMs])
            .from(RateLimits::Table)
            .and_where(Expr::col(RateLimits::Bucket).eq(bucket))
            .to_owned(),
    )
}

pub fn rate_put(bucket: &str, tokens: f64, updated_ms: i64) -> Q {
    sqlite(
        &Query::insert()
            .into_table(RateLimits::Table)
            .columns([RateLimits::Bucket, RateLimits::Tokens, RateLimits::UpdatedMs])
            .values_panic([bucket.into(), tokens.into(), updated_ms.into()])
            .on_conflict(
                OnConflict::column(RateLimits::Bucket)
                    .update_columns([RateLimits::Tokens, RateLimits::UpdatedMs])
                    .to_owned(),
            )
            .to_owned(),
    )
}

/// Several buckets in one read (the IP and network buckets of a request).
pub fn rate_get_many(buckets: &[String]) -> Q {
    sqlite(
        &Query::select()
            .columns([RateLimits::Bucket, RateLimits::Tokens, RateLimits::UpdatedMs])
            .from(RateLimits::Table)
            .and_where(Expr::col(RateLimits::Bucket).is_in(buckets.iter().map(String::as_str)))
            .to_owned(),
    )
}

pub fn rate_cleanup(before_ms: i64) -> Q {
    sqlite(
        &Query::delete()
            .from_table(RateLimits::Table)
            .and_where(Expr::col(RateLimits::UpdatedMs).lt(before_ms))
            .to_owned(),
    )
}

// ---------------------------------------------------------- contributions ---

/// Rows of one contribution, as JSON arrays: texts `[{k, en}]`, pairs
/// `[{g, k}]`, translations `[{id, k, tr, src}]`.
pub struct ContributionRows<'a> {
    pub texts: &'a str,
    pub pairs: &'a str,
    pub trs: &'a str,
    pub ip_hash: &'a str,
    pub lang: &'a str,
    pub now: &'a str,
    pub game_build: Option<i64>,
}

/// Different translations one text can collect from players' uploads (per language); more are refused, so a text
/// cannot pile up variants.
pub const MAX_TRANSLATIONS_PER_TEXT: i64 = 10;

/// Every statement that stores a contribution (run as one D1 batch).
pub fn contribution(c: &ContributionRows) -> Vec<Q> {
    let mut out = Vec::new();
    // Texts and their distinct reporters.
    out.push(sqlite(
        &Query::insert()
            .into_table(Texts::Table)
            .columns([Texts::Key, Texts::En, Texts::CreatedAt])
            .select_from(json_pairs(c.texts, "k", "en").expr(Expr::val(c.now)).to_owned())
            .expect("3 columns")
            .on_conflict(do_nothing())
            .to_owned(),
    ));
    out.push(sqlite(
        &Query::insert()
            .into_table(TextReporters::Table)
            .columns([TextReporters::TextKey, TextReporters::IpHash])
            .select_from(json_list(c.texts, Some("k")).expr(Expr::val(c.ip_hash)).to_owned())
            .expect("2 columns")
            .on_conflict(do_nothing())
            .to_owned(),
    ));
    out.push(sqlite(
        &Query::update()
            .table(Texts::Table)
            .value(
                Texts::Reporters,
                count_where(
                    TextReporters::Table,
                    Cond::all().add(
                        Expr::col((TextReporters::Table, TextReporters::TextKey)).equals((Texts::Table, Texts::Key)),
                    ),
                ),
            )
            .and_where(Expr::col(Texts::Key).in_subquery(json_list(c.texts, Some("k"))))
            .to_owned(),
    ));
    // ID pairs: rejected at once when they conflict with a trusted pair.
    let conflict = Query::select()
        .expr(Expr::val(1))
        .from_as(IdPairs::Table, P)
        .and_where(Expr::col((P, IdPairs::GameId)).eq(jf("g")))
        .and_where(Expr::col((P, IdPairs::TextKey)).ne(jf("k")))
        .and_where(Expr::col((P, IdPairs::State)).eq("trusted"))
        .to_owned();
    out.push(sqlite(
        &Query::insert()
            .into_table(IdPairs::Table)
            .columns([
                IdPairs::GameId,
                IdPairs::TextKey,
                IdPairs::State,
                IdPairs::FirstBuild,
                IdPairs::LastBuild,
                IdPairs::CreatedAt,
            ])
            .select_from(
                json_pairs(c.pairs, "g", "k")
                    .expr(Expr::case(Expr::exists(conflict), "rejected").finally("pending"))
                    .expr(Expr::val(c.game_build))
                    .expr(Expr::val(c.game_build))
                    .expr(Expr::val(c.now))
                    .to_owned(),
            )
            .expect("6 columns")
            .on_conflict(do_nothing())
            .to_owned(),
    ));
    out.push(sqlite(
        &Query::insert()
            .into_table(IdPairReporters::Table)
            .columns([
                IdPairReporters::GameId,
                IdPairReporters::TextKey,
                IdPairReporters::IpHash,
            ])
            .select_from(json_pairs(c.pairs, "g", "k").expr(Expr::val(c.ip_hash)).to_owned())
            .expect("3 columns")
            .on_conflict(do_nothing())
            .to_owned(),
    ));
    out.push(sqlite(
        &Query::update()
            .table(IdPairs::Table)
            .value(
                IdPairs::Reporters,
                count_where(
                    IdPairReporters::Table,
                    Cond::all()
                        .add(
                            Expr::col((IdPairReporters::Table, IdPairReporters::GameId))
                                .equals((IdPairs::Table, IdPairs::GameId)),
                        )
                        .add(
                            Expr::col((IdPairReporters::Table, IdPairReporters::TextKey))
                                .equals((IdPairs::Table, IdPairs::TextKey)),
                        ),
                ),
            )
            .value(
                IdPairs::LastBuild,
                Func::cust(MaxScalar).args([
                    Func::coalesce([Expr::col(IdPairs::LastBuild), Expr::val(0)]).into(),
                    Func::coalesce([Expr::val(c.game_build), Expr::val(0)]).into(),
                ]),
            )
            .and_where(
                Expr::tuple([Expr::col(IdPairs::GameId), Expr::col(IdPairs::TextKey)])
                    .in_subquery(json_pairs(c.pairs, "g", "k")),
            )
            .to_owned(),
    ));
    // Translations and their distinct reporters. A new translation is refused when the text already has one made by
    // the server's AI (better than any local model) or already has MAX_TRANSLATIONS_PER_TEXT variants.
    let ai_done = Query::select()
        .expr(Expr::val(1))
        .from_as(Translations::Table, Y)
        .and_where(Expr::col((Y, Translations::TextKey)).eq(jf("k")))
        .and_where(Expr::col((Y, Translations::Lang)).eq(c.lang))
        .and_where(Expr::col((Y, Translations::Source)).eq("ai"))
        .and_where(Expr::col((Y, Translations::Review)).ne("hidden"))
        .to_owned();
    let variants = Query::select()
        .expr(Func::count(Expr::col((Y, Translations::Id))))
        .from_as(Translations::Table, Y)
        .and_where(Expr::col((Y, Translations::TextKey)).eq(jf("k")))
        .and_where(Expr::col((Y, Translations::Lang)).eq(c.lang))
        // Hidden ones (moderated away) do not use up the text's room.
        .and_where(Expr::col((Y, Translations::Review)).ne("hidden"))
        .to_owned();
    out.push(sqlite(
        &Query::insert()
            .into_table(Translations::Table)
            .columns([
                Translations::Id,
                Translations::TextKey,
                Translations::Lang,
                Translations::Tr,
                Translations::Source,
                Translations::CreatedAt,
            ])
            .select_from(
                from_json(c.trs)
                    .expr(jf("id"))
                    .expr(jf("k"))
                    .expr(Expr::val(c.lang))
                    .expr(jf("tr"))
                    .expr(jf("src"))
                    .expr(Expr::val(c.now))
                    .and_where(Expr::not_exists(ai_done))
                    .and_where(Expr::expr(Expr::SubQuery(None, Box::new(variants.into()))).lt(MAX_TRANSLATIONS_PER_TEXT))
                    .to_owned(),
            )
            .expect("6 columns")
            .on_conflict(do_nothing())
            .to_owned(),
    ));
    // Reporters only for translations that exist (a refused one leaves nothing behind).
    let stored = Query::select().column(Translations::Id).from(Translations::Table).to_owned();
    out.push(sqlite(
        &Query::insert()
            .into_table(TranslationReporters::Table)
            .columns([TranslationReporters::TranslationId, TranslationReporters::IpHash])
            .select_from(
                json_list(c.trs, Some("id"))
                    .expr(Expr::val(c.ip_hash))
                    .and_where(jf("id").in_subquery(stored))
                    .to_owned(),
            )
            .expect("2 columns")
            .on_conflict(do_nothing())
            .to_owned(),
    ));
    out.push(recount_translation_reporters(
        Expr::col(Translations::Id).in_subquery(json_list(c.trs, Some("id"))),
    ));
    // Review queue.
    out.push(enqueue_select(
        "text",
        Query::select()
            .expr(Expr::val("text"))
            .column(Texts::Key)
            .expr(Expr::val(c.now))
            .from(Texts::Table)
            .and_where(Expr::col(Texts::State).eq("pending"))
            .and_where(Expr::col(Texts::Key).in_subquery(json_list(c.texts, Some("k"))))
            .to_owned(),
    ));
    out.push(enqueue_select(
        "pair",
        Query::select()
            .expr(Expr::val("pair"))
            .expr(pair_ref(IdPairs::Table))
            .expr(Expr::val(c.now))
            .from(IdPairs::Table)
            .and_where(Expr::col(IdPairs::State).eq("pending"))
            .and_where(
                Expr::tuple([Expr::col(IdPairs::GameId), Expr::col(IdPairs::TextKey)])
                    .in_subquery(json_pairs(c.pairs, "g", "k")),
            )
            .to_owned(),
    ));
    // Machine translations from players are only reviewed while the text has
    // no reviewed translation yet (site corrections are always reviewed).
    let has_passed = Query::select()
        .expr(Expr::val(1))
        .from_as(Translations::Table, Y)
        .and_where(Expr::col((Y, Translations::TextKey)).equals((X, Translations::TextKey)))
        .and_where(Expr::col((Y, Translations::Lang)).equals((X, Translations::Lang)))
        .and_where(Expr::col((Y, Translations::Review)).eq("passed"))
        .to_owned();
    out.push(enqueue_select(
        "translation",
        Query::select()
            .expr(Expr::val("translation"))
            .column((X, Translations::Id))
            .expr(Expr::val(c.now))
            .from_as(Translations::Table, X)
            .and_where(Expr::col((X, Translations::Review)).eq("pending"))
            .and_where(Expr::col((X, Translations::Id)).in_subquery(json_list(c.trs, Some("id"))))
            .and_where(Expr::not_exists(has_passed))
            .to_owned(),
    ));
    out
}

/// `game_id || ':' || text_key` of an `id_pairs` row (queue reference).
fn pair_ref(table: impl Iden + Copy + 'static) -> Expr {
    Expr::col((table, IdPairs::GameId))
        .binary(sea_query::BinOper::Custom("||"), Expr::val(":"))
        .binary(sea_query::BinOper::Custom("||"), Expr::col((table, IdPairs::TextKey)))
}

/// `INSERT INTO review_queue (kind, ref, enqueued_at) <select> ON CONFLICT DO NOTHING`.
fn enqueue_select(_kind: &str, select: SelectStatement) -> Q {
    sqlite(
        &Query::insert()
            .into_table(ReviewQueue::Table)
            .columns([ReviewQueue::Kind, ReviewQueue::Ref, ReviewQueue::EnqueuedAt])
            .select_from(select)
            .expect("3 columns")
            .on_conflict(do_nothing())
            .to_owned(),
    )
}

fn recount_translation_reporters(filter: Expr) -> Q {
    sqlite(
        &Query::update()
            .table(Translations::Table)
            .value(
                Translations::Reporters,
                count_where(
                    TranslationReporters::Table,
                    Cond::all().add(
                        Expr::col((TranslationReporters::Table, TranslationReporters::TranslationId))
                            .equals((Translations::Table, Translations::Id)),
                    ),
                ),
            )
            .and_where(filter)
            .to_owned(),
    )
}

// ------------------------------------------------------- votes / current ---

pub fn vote_insert(translation_id: &str, ip_hash: &str, value: i64, now: &str) -> Q {
    sqlite(
        &Query::insert()
            .into_table(Votes::Table)
            .columns([Votes::TranslationId, Votes::IpHash, Votes::Value, Votes::CreatedAt])
            .values_panic([translation_id.into(), ip_hash.into(), value.into(), now.into()])
            .on_conflict(
                OnConflict::columns([Votes::TranslationId, Votes::IpHash])
                    .do_nothing()
                    .to_owned(),
            )
            .to_owned(),
    )
}

/// Recounts up/down votes of the translations listed in `ids_json`.
pub fn recount_votes(ids_json: &str) -> Q {
    let count = |value: i64| {
        count_where(
            Votes::Table,
            Cond::all()
                .add(Expr::col((Votes::Table, Votes::TranslationId)).equals((Translations::Table, Translations::Id)))
                .add(Expr::col((Votes::Table, Votes::Value)).eq(value)),
        )
    };
    sqlite(
        &Query::update()
            .table(Translations::Table)
            .value(Translations::Up, count(1))
            .value(Translations::Down, count(-1))
            .and_where(Expr::col(Translations::Id).in_subquery(json_list(ids_json, None)))
            .to_owned(),
    )
}

pub fn translations_votes(ids_json: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([Translations::Id, Translations::Up, Translations::Down])
            .from(Translations::Table)
            .and_where(Expr::col(Translations::Id).in_subquery(json_list(ids_json, None)))
            .to_owned(),
    )
}

pub fn set_score(id: &str, score: f64) -> Q {
    sqlite(
        &Query::update()
            .table(Translations::Table)
            .value(Translations::Score, score)
            .and_where(Expr::col(Translations::Id).eq(id))
            .to_owned(),
    )
}

/// Passed translations of the `(text_key, lang)` pairs in `keys_json` (`[{k, l}]`).
pub fn current_candidates(keys_json: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([
                Translations::Id,
                Translations::TextKey,
                Translations::Lang,
                Translations::Review,
                Translations::Up,
                Translations::Down,
                Translations::PassedAt,
            ])
            .from(Translations::Table)
            .and_where(Expr::col(Translations::Review).eq("passed"))
            .and_where(
                Expr::tuple([Expr::col(Translations::TextKey), Expr::col(Translations::Lang)])
                    .in_subquery(json_pairs(keys_json, "k", "l")),
            )
            .to_owned(),
    )
}

pub struct CurrentRow<'a> {
    pub text_key: &'a str,
    pub lang: &'a str,
    pub translation_id: &'a str,
    pub score: f64,
    pub up: i64,
    pub down: i64,
    pub now: &'a str,
}

/// Upserts the current translation; `updated_at` only moves when it changes.
pub fn current_upsert(r: &CurrentRow) -> Q {
    let changed = Expr::col((Current::Table, Current::TranslationId)).ne(Expr::col((Excluded, Current::TranslationId)));
    sqlite(
        &Query::insert()
            .into_table(Current::Table)
            .columns([
                Current::TextKey,
                Current::Lang,
                Current::TranslationId,
                Current::Score,
                Current::Up,
                Current::Down,
                Current::UpdatedAt,
            ])
            .values_panic([
                r.text_key.into(),
                r.lang.into(),
                r.translation_id.into(),
                r.score.into(),
                r.up.into(),
                r.down.into(),
                r.now.into(),
            ])
            .on_conflict(
                OnConflict::columns([Current::TextKey, Current::Lang])
                    .value(
                        Current::UpdatedAt,
                        Expr::case(changed, Expr::col((Excluded, Current::UpdatedAt)))
                            .finally(Expr::col((Current::Table, Current::UpdatedAt))),
                    )
                    .update_columns([Current::TranslationId, Current::Score, Current::Up, Current::Down])
                    .to_owned(),
            )
            .to_owned(),
    )
}

pub fn current_delete(text_key: &str, lang: &str) -> Q {
    sqlite(
        &Query::delete()
            .from_table(Current::Table)
            .and_where(Expr::col(Current::TextKey).eq(text_key))
            .and_where(Expr::col(Current::Lang).eq(lang))
            .to_owned(),
    )
}

/// The text of the current translation of a text in a language.
pub fn current_tr(text_key: &str, lang: &str) -> Q {
    sqlite(
        &Query::select()
            .column((Translations::Table, Translations::Tr))
            .from(Current::Table)
            .inner_join(
                Translations::Table,
                Expr::col((Translations::Table, Translations::Id)).equals((Current::Table, Current::TranslationId)),
            )
            .and_where(Expr::col((Current::Table, Current::TextKey)).eq(text_key))
            .and_where(Expr::col((Current::Table, Current::Lang)).eq(lang))
            .to_owned(),
    )
}

pub fn translation_target(id: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([Translations::TextKey, Translations::Lang, Translations::Review])
            .from(Translations::Table)
            .and_where(Expr::col(Translations::Id).eq(id))
            .to_owned(),
    )
}

/// Votes of a translation and whether it is current.
pub fn vote_result(id: &str) -> Q {
    let is_current = Query::select()
        .expr(Expr::val(1))
        .from_as(Current::Table, C)
        .and_where(Expr::col((C, Current::TranslationId)).equals((X, Translations::Id)))
        .to_owned();
    sqlite(
        &Query::select()
            .column((X, Translations::Up))
            .column((X, Translations::Down))
            .expr_as(Expr::exists(is_current), Alias::new("current"))
            .from_as(Translations::Table, X)
            .and_where(Expr::col((X, Translations::Id)).eq(id))
            .to_owned(),
    )
}

pub fn counter_get(name: &str) -> Q {
    sqlite(
        &Query::select()
            .column(Counters::Value)
            .from(Counters::Table)
            .and_where(Expr::col(Counters::Name).eq(name))
            .to_owned(),
    )
}

pub fn counter_increment(name: &str) -> Q {
    sqlite(
        &Query::update()
            .table(Counters::Table)
            .value(Counters::Value, Expr::col(Counters::Value).add(1))
            .and_where(Expr::col(Counters::Name).eq(name))
            .to_owned(),
    )
}

/// Reserves `units` of a daily budget counter: adds them only while the total stays at most `cap`, and returns the
/// new total (no row when refused, and then nothing is written). The caller never asks for more than `cap` at once.
pub fn budget_reserve(name: &str, units: i64, cap: i64) -> Q {
    let total = Expr::col((Counters::Table, Counters::Value)).add(Expr::col((Excluded, Counters::Value)));
    sqlite(
        &Query::insert()
            .into_table(Counters::Table)
            .columns([Counters::Name, Counters::Value])
            .values_panic([name.into(), units.into()])
            .on_conflict(
                OnConflict::column(Counters::Name)
                    .value(Counters::Value, total.clone())
                    .action_and_where(total.lte(cap))
                    .to_owned(),
            )
            .returning_col(Counters::Value)
            .to_owned(),
    )
}

/// Adds `delta` (may be negative: a refund of an over-estimate) to a daily budget counter, never below 0, and
/// returns the new total.
pub fn budget_add(name: &str, delta: i64) -> Q {
    sqlite(
        &Query::insert()
            .into_table(Counters::Table)
            .columns([Counters::Name, Counters::Value])
            .values_panic([name.into(), std::cmp::Ord::max(delta, 0).into()])
            .on_conflict(
                OnConflict::column(Counters::Name)
                    .value(
                        Counters::Value,
                        Func::cust(MaxScalar).args([
                            Expr::col((Counters::Table, Counters::Value)).add(delta),
                            Expr::val(0),
                        ]),
                    )
                    .to_owned(),
            )
            .returning_col(Counters::Value)
            .to_owned(),
    )
}

// ------------------------------------------------------------- site reads ---

/// `sort` of the text listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListSort {
    Worst,
    Best,
    Recent,
    Oldest,
}

/// `down - up` of the current translation; the `current_net` index is on this exact expression.
fn net_votes() -> Expr {
    Expr::col((C, Current::Down)).sub(Expr::col((C, Current::Up)))
}

/// Texts for `GET /v1/texts`, one page (`limit` rows after `offset`).
///
/// Without `search`: the texts with a current translation, ordered by one sort key and then the text key, each
/// sort walking one index of `current` from its `lang` end (`current_updated`, `current_score`, `current_net`, see
/// migration 0002), so no page needs a sort of the whole table:
/// - `Recent`: `updated_at` newest first; `Oldest`: oldest first;
/// - `Best`: score (Wilson lower bound) highest first;
/// - `Worst`: down minus up votes, highest first.
///
/// With `search` (a `LIKE` pattern): any trusted text whose English or current translation matches, in a similar
/// order; this scans, so callers only serve its first pages (`api::MAX_SEARCH_OFFSET`).
pub fn list_texts(lang: &str, sort: ListSort, search: Option<&str>, limit: u64, offset: u64) -> Q {
    let mut s = Query::select();
    s.column((T, Texts::Key))
        .column((T, Texts::En))
        .expr_as(Expr::col((C, Current::TranslationId)), Alias::new("cur"));
    match search {
        None => {
            // `current` drives the join (in SQLite a CROSS JOIN keeps the written table order, so the planner never
            // starts at `texts`), read in index order. The join condition is in WHERE: CROSS JOIN takes no ON.
            s.from_as(Current::Table, C)
                .join_as(JoinType::CrossJoin, Texts::Table, T, Cond::all())
                .and_where(Expr::col((T, Texts::Key)).equals((C, Current::TextKey)))
                .and_where(Expr::col((C, Current::Lang)).eq(lang))
                .and_where(Expr::col((T, Texts::State)).eq("trusted"));
            let (key, dir): (Expr, Order) = match sort {
                ListSort::Recent => (Expr::col((C, Current::UpdatedAt)), Order::Desc),
                ListSort::Oldest => (Expr::col((C, Current::UpdatedAt)), Order::Asc),
                ListSort::Best => (Expr::col((C, Current::Score)), Order::Desc),
                ListSort::Worst => (net_votes(), Order::Desc),
            };
            s.order_by_expr(key, dir.clone()).order_by((C, Current::TextKey), dir);
        }
        Some(pattern) => {
            let on_current = Cond::all()
                .add(Expr::col((C, Current::TextKey)).equals((T, Texts::Key)))
                .add(Expr::col((C, Current::Lang)).eq(lang));
            s.from_as(Texts::Table, T)
                .join_as(JoinType::LeftJoin, Current::Table, C, on_current)
                .join_as(
                    JoinType::LeftJoin,
                    Translations::Table,
                    X,
                    Expr::col((X, Translations::Id)).equals((C, Current::TranslationId)),
                )
                .cond_where(
                    Cond::any()
                        .add(Expr::col((T, Texts::En)).like(sea_query::LikeExpr::new(pattern).escape('\\')))
                        .add(Expr::col((X, Translations::Tr)).like(sea_query::LikeExpr::new(pattern).escape('\\'))),
                )
                .and_where(Expr::col((T, Texts::State)).eq("trusted"));
            let updated = || Func::coalesce([Expr::col((C, Current::UpdatedAt)), Expr::col((T, Texts::CreatedAt))]);
            match sort {
                ListSort::Worst => s.order_by_expr(Func::coalesce([net_votes(), Expr::val(0)]).into(), Order::Desc),
                ListSort::Best => s.order_by_expr(
                    Func::coalesce([Expr::col((C, Current::Score)), Expr::val(-1)]).into(),
                    Order::Desc,
                ),
                ListSort::Recent => s.order_by_expr(updated().into(), Order::Desc),
                ListSort::Oldest => s.order_by_expr(updated().into(), Order::Asc),
            };
            s.order_by((T, Texts::Key), Order::Asc);
        }
    }
    s.limit(limit).offset(offset);
    sqlite(&s)
}

/// Passed translations of the text keys in `keys_json`, best first.
pub fn passed_translations(lang: &str, keys_json: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([
                Translations::Id,
                Translations::TextKey,
                Translations::Tr,
                Translations::Source,
                Translations::Up,
                Translations::Down,
                Translations::Score,
            ])
            .from(Translations::Table)
            .and_where(Expr::col(Translations::Lang).eq(lang))
            .and_where(Expr::col(Translations::Review).eq("passed"))
            .and_where(Expr::col(Translations::TextKey).in_subquery(json_list(keys_json, None)))
            .order_by(Translations::Score, Order::Desc)
            .order_by(Translations::PassedAt, Order::Asc)
            .to_owned(),
    )
}

/// `group_concat(game_id)` of trusted pairs per text key in `keys_json`.
pub fn trusted_ids(keys_json: &str) -> Q {
    sqlite(
        &Query::select()
            .column(IdPairs::TextKey)
            .expr_as(
                Func::cust(GroupConcat).arg(Expr::col(IdPairs::GameId)),
                Alias::new("ids"),
            )
            .from(IdPairs::Table)
            .and_where(Expr::col(IdPairs::State).eq("trusted"))
            .and_where(Expr::col(IdPairs::TextKey).in_subquery(json_list(keys_json, None)))
            .group_by_col(IdPairs::TextKey)
            .to_owned(),
    )
}

pub fn text_get(key: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([Texts::En, Texts::State])
            .from(Texts::Table)
            .and_where(Expr::col(Texts::Key).eq(key))
            .to_owned(),
    )
}

/// Statements that store a site correction (one D1 batch).
pub fn correction(id: &str, text_key: &str, lang: &str, tr: &str, ip_hash: &str, now: &str) -> Vec<Q> {
    vec![
        sqlite(
            &Query::insert()
                .into_table(Translations::Table)
                .columns([
                    Translations::Id,
                    Translations::TextKey,
                    Translations::Lang,
                    Translations::Tr,
                    Translations::Source,
                    Translations::CreatedAt,
                ])
                .values_panic([
                    id.into(),
                    text_key.into(),
                    lang.into(),
                    tr.into(),
                    "user".into(),
                    now.into(),
                ])
                .on_conflict(do_nothing())
                .to_owned(),
        ),
        sqlite(
            &Query::insert()
                .into_table(TranslationReporters::Table)
                .columns([TranslationReporters::TranslationId, TranslationReporters::IpHash])
                .values_panic([id.into(), ip_hash.into()])
                .on_conflict(do_nothing())
                .to_owned(),
        ),
        recount_translation_reporters(Expr::col(Translations::Id).eq(id)),
        enqueue_translation_if_pending(id, now),
    ]
}

fn enqueue_translation_if_pending(id: &str, now: &str) -> Q {
    enqueue_select(
        "translation",
        Query::select()
            .expr(Expr::val("translation"))
            .column(Translations::Id)
            .expr(Expr::val(now))
            .from(Translations::Table)
            .and_where(Expr::col(Translations::Id).eq(id))
            .and_where(Expr::col(Translations::Review).eq("pending"))
            .to_owned(),
    )
}

pub fn active_manifest(lang: &str) -> Q {
    sqlite(
        &Query::select()
            .column(Releases::Manifest)
            .from(Releases::Table)
            .and_where(Expr::col(Releases::Lang).eq(lang))
            .and_where(Expr::col(Releases::Active).eq(1))
            .limit(1)
            .to_owned(),
    )
}

// ----------------------------------------------------------------- review ---

pub fn queue_due(now_ms: i64, limit: u64) -> Q {
    sqlite(
        &Query::select()
            .columns([
                ReviewQueue::Id,
                ReviewQueue::Kind,
                ReviewQueue::Ref,
                ReviewQueue::Attempts,
            ])
            .from(ReviewQueue::Table)
            .and_where(Expr::col(ReviewQueue::NotBeforeMs).lte(now_ms))
            // In the order of `review_queue_next`, so a run reads only the rows it leases, however long the queue
            // (new items have `not_before_ms` 0 and come first, oldest first).
            .order_by(ReviewQueue::NotBeforeMs, Order::Asc)
            .order_by(ReviewQueue::Id, Order::Asc)
            .limit(limit)
            .to_owned(),
    )
}

pub fn queue_lease(ids_json: &str, until_ms: i64) -> Q {
    sqlite(
        &Query::update()
            .table(ReviewQueue::Table)
            .value(ReviewQueue::Attempts, Expr::col(ReviewQueue::Attempts).add(1))
            .value(ReviewQueue::NotBeforeMs, until_ms)
            .and_where(Expr::col(ReviewQueue::Id).in_subquery(json_list(ids_json, None)))
            .to_owned(),
    )
}

pub fn queue_delete(ids_json: &str) -> Q {
    sqlite(
        &Query::delete()
            .from_table(ReviewQueue::Table)
            .and_where(Expr::col(ReviewQueue::Id).in_subquery(json_list(ids_json, None)))
            .to_owned(),
    )
}

pub fn queue_retry(id: i64, not_before_ms: i64) -> Q {
    sqlite(
        &Query::update()
            .table(ReviewQueue::Table)
            .value(ReviewQueue::NotBeforeMs, not_before_ms)
            .and_where(Expr::col(ReviewQueue::Id).eq(id))
            .to_owned(),
    )
}

pub fn texts_by_keys(keys_json: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([Texts::Key, Texts::En, Texts::State, Texts::Reporters])
            .from(Texts::Table)
            .and_where(Expr::col(Texts::Key).in_subquery(json_list(keys_json, None)))
            .to_owned(),
    )
}

/// Translations with their English text and its state.
pub fn translations_for_review(ids_json: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([
                (X, Translations::Id),
                (X, Translations::TextKey),
                (X, Translations::Lang),
                (X, Translations::Tr),
                (X, Translations::Review),
            ])
            .column((T, Texts::En))
            .expr_as(Expr::col((T, Texts::State)), Alias::new("text_state"))
            .from_as(Translations::Table, X)
            .join_as(
                JoinType::InnerJoin,
                Texts::Table,
                T,
                Expr::col((T, Texts::Key)).equals((X, Translations::TextKey)),
            )
            .and_where(Expr::col((X, Translations::Id)).in_subquery(json_list(ids_json, None)))
            .to_owned(),
    )
}

/// Sets a pending text's state.
pub fn set_text_state(key: &str, state: &str, note: &str, now: &str) -> Q {
    sqlite(
        &Query::update()
            .table(Texts::Table)
            .values([
                (Texts::State, state.into()),
                (Texts::ReviewNote, note.into()),
                (Texts::ReviewedAt, now.into()),
            ])
            .and_where(Expr::col(Texts::Key).eq(key))
            .and_where(Expr::col(Texts::State).eq("pending"))
            .to_owned(),
    )
}

/// Sets a pending translation's review state (`passed_at` when it passes).
pub fn set_review(id: &str, review: &str, note: &str, now: &str) -> Q {
    let mut u = Query::update();
    u.table(Translations::Table)
        .value(Translations::Review, review)
        .value(Translations::ReviewNote, note);
    if review == "passed" {
        u.value(Translations::PassedAt, now);
    }
    sqlite(
        &u.and_where(Expr::col(Translations::Id).eq(id))
            .and_where(Expr::col(Translations::Review).eq("pending"))
            .to_owned(),
    )
}

/// Queues the pending pairs of the text keys in `keys_json`.
pub fn enqueue_pairs_of_texts(keys_json: &str, now: &str) -> Q {
    enqueue_select(
        "pair",
        Query::select()
            .expr(Expr::val("pair"))
            .expr(pair_ref(IdPairs::Table))
            .expr(Expr::val(now))
            .from(IdPairs::Table)
            .and_where(Expr::col(IdPairs::State).eq("pending"))
            .and_where(Expr::col(IdPairs::TextKey).in_subquery(json_list(keys_json, None)))
            .to_owned(),
    )
}

/// All candidate pairs (with text state) of the game IDs in `ids_json`.
pub fn pairs_for_ids(ids_json: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([
                (P, IdPairs::GameId),
                (P, IdPairs::TextKey),
                (P, IdPairs::Reporters),
                (P, IdPairs::State),
            ])
            .expr_as(Expr::col((T, Texts::State)), Alias::new("text_state"))
            .from_as(IdPairs::Table, P)
            .join_as(
                JoinType::InnerJoin,
                Texts::Table,
                T,
                Expr::col((T, Texts::Key)).equals((P, IdPairs::TextKey)),
            )
            .and_where(Expr::col((P, IdPairs::GameId)).in_subquery(json_list(ids_json, None)))
            .order_by((P, IdPairs::GameId), Order::Asc)
            .order_by((P, IdPairs::CreatedAt), Order::Asc)
            .to_owned(),
    )
}

pub fn set_pair_state(game_id: i64, text_key: &str, state: &str, now: &str) -> Q {
    sqlite(
        &Query::update()
            .table(IdPairs::Table)
            .values([(IdPairs::State, state.into()), (IdPairs::ReviewedAt, now.into())])
            .and_where(Expr::col(IdPairs::GameId).eq(game_id))
            .and_where(Expr::col(IdPairs::TextKey).eq(text_key))
            .to_owned(),
    )
}

// ------------------------------------------------------------ stats / kv ---

pub fn stats(lang: &str, perfect_min_votes: i64, perfect_min_score: f64) -> Q {
    let trusted = || {
        Query::select()
            .expr(Func::count(Expr::val(1)))
            .from(Texts::Table)
            .and_where(Expr::col(Texts::State).eq("trusted"))
            .to_owned()
    };
    let current = |perfect: bool| {
        let mut s = Query::select()
            .expr(Func::count(Expr::val(1)))
            .from_as(Current::Table, C)
            .join_as(
                JoinType::InnerJoin,
                Texts::Table,
                T,
                Expr::col((T, Texts::Key)).equals((C, Current::TextKey)),
            )
            .and_where(Expr::col((C, Current::Lang)).eq(lang))
            .and_where(Expr::col((T, Texts::State)).eq("trusted"))
            .to_owned();
        if perfect {
            s.and_where(
                Expr::col((C, Current::Up))
                    .add(Expr::col((C, Current::Down)))
                    .gte(perfect_min_votes),
            )
            .and_where(Expr::col((C, Current::Score)).gte(perfect_min_score));
        }
        s
    };
    let queue = Query::select()
        .expr(Func::count(Expr::val(1)))
        .from(ReviewQueue::Table)
        .to_owned();
    sqlite(
        &Query::select()
            .expr_as(trusted(), Alias::new("texts"))
            .expr_as(current(false), Alias::new("translated"))
            .expr_as(current(true), Alias::new("perfect"))
            .expr_as(queue, Alias::new("pending"))
            .to_owned(),
    )
}

pub fn kv_get(key: &str) -> Q {
    sqlite(
        &Query::select()
            .column(Kv::Value)
            .from(Kv::Table)
            .and_where(Expr::col(Kv::Key).eq(key))
            .to_owned(),
    )
}

pub fn kv_put(key: &str, value: &str, now: &str) -> Q {
    sqlite(
        &Query::insert()
            .into_table(Kv::Table)
            .columns([Kv::Key, Kv::Value, Kv::UpdatedAt])
            .values_panic([key.into(), value.into(), now.into()])
            .on_conflict(
                OnConflict::column(Kv::Key)
                    .update_columns([Kv::Value, Kv::UpdatedAt])
                    .to_owned(),
            )
            .to_owned(),
    )
}

pub fn kv_delete(key: &str) -> Q {
    sqlite(
        &Query::delete()
            .from_table(Kv::Table)
            .and_where(Expr::col(Kv::Key).eq(key))
            .to_owned(),
    )
}

// --------------------------------------------------------------- releases ---

/// One page of release records (keyset by text key).
pub fn release_page(lang: &str, after: &str, limit: u64) -> Q {
    let ids = Query::select()
        .expr(Func::cust(GroupConcat).arg(Expr::col((P, IdPairs::GameId))))
        .from_as(IdPairs::Table, P)
        .and_where(Expr::col((P, IdPairs::TextKey)).equals((C, Current::TextKey)))
        .and_where(Expr::col((P, IdPairs::State)).eq("trusted"))
        .to_owned();
    // `current` is read in primary key order from `after` on: the CROSS JOINs keep it the outer table, and the unary
    // `+` keeps the planner off the `lang` indexes. Without both, every page read and sorted the whole language
    // (~900k rows read and ~6.5 s per page on D1), and the daily release job hit D1's "overloaded" limit.
    sqlite(
        &Query::select()
            .column((C, Current::TextKey))
            .column((T, Texts::En))
            .column((X, Translations::Tr))
            .column((X, Translations::Source))
            .expr_as(ids, Alias::new("ids"))
            .from_as(Current::Table, C)
            .join_as(JoinType::CrossJoin, Texts::Table, T, Cond::all())
            .join_as(JoinType::CrossJoin, Translations::Table, X, Cond::all())
            .and_where(Expr::col((T, Texts::Key)).equals((C, Current::TextKey)))
            .and_where(Expr::col((X, Translations::Id)).equals((C, Current::TranslationId)))
            .and_where(Expr::cust(r#"+"c"."lang""#).eq(lang))
            .and_where(Expr::col((C, Current::TextKey)).gt(after))
            .and_where(Expr::col((T, Texts::State)).eq("trusted"))
            .and_where(Expr::col((X, Translations::Review)).eq("passed"))
            .order_by((C, Current::TextKey), Order::Asc)
            .limit(limit)
            .to_owned(),
    )
}

pub fn active_release(lang: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([Releases::Version, Releases::ContentHash])
            .from(Releases::Table)
            .and_where(Expr::col(Releases::Lang).eq(lang))
            .and_where(Expr::col(Releases::Active).eq(1))
            .to_owned(),
    )
}

pub fn max_version(lang: &str) -> Q {
    sqlite(
        &Query::select()
            .expr_as(Func::max(Expr::col(Releases::Version)), Alias::new("v"))
            .from(Releases::Table)
            .and_where(Expr::col(Releases::Lang).eq(lang))
            .to_owned(),
    )
}

pub struct NewRelease<'a> {
    pub lang: &'a str,
    pub version: i64,
    pub created_at: &'a str,
    pub files: &'a str,
    pub manifest: &'a str,
    pub content_hash: &'a str,
    pub records: i64,
}

/// Deactivates the old release and inserts the new active one (one batch).
pub fn release_insert(r: &NewRelease) -> Vec<Q> {
    vec![
        sqlite(
            &Query::update()
                .table(Releases::Table)
                .value(Releases::Active, 0)
                .and_where(Expr::col(Releases::Lang).eq(r.lang))
                .to_owned(),
        ),
        sqlite(
            &Query::insert()
                .into_table(Releases::Table)
                .columns([
                    Releases::Lang,
                    Releases::Version,
                    Releases::CreatedAt,
                    Releases::Files,
                    Releases::Manifest,
                    Releases::ContentHash,
                    Releases::Records,
                    Releases::Active,
                ])
                .values_panic([
                    r.lang.into(),
                    r.version.into(),
                    r.created_at.into(),
                    r.files.into(),
                    r.manifest.into(),
                    r.content_hash.into(),
                    r.records.into(),
                    1.into(),
                ])
                .to_owned(),
        ),
    ]
}

pub fn release_list(lang: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([
                Releases::Version,
                Releases::CreatedAt,
                Releases::Records,
                Releases::Active,
                Releases::ContentHash,
            ])
            .from(Releases::Table)
            .and_where(Expr::col(Releases::Lang).eq(lang))
            .order_by(Releases::Version, Order::Desc)
            .to_owned(),
    )
}

pub fn release_exists(lang: &str, version: i64) -> Q {
    sqlite(
        &Query::select()
            .column(Releases::Version)
            .from(Releases::Table)
            .and_where(Expr::col(Releases::Lang).eq(lang))
            .and_where(Expr::col(Releases::Version).eq(version))
            .to_owned(),
    )
}

/// `active = (version = ?)` for every release of `lang`.
pub fn release_activate(lang: &str, version: i64) -> Q {
    sqlite(
        &Query::update()
            .table(Releases::Table)
            .value(Releases::Active, Expr::col(Releases::Version).eq(version))
            .and_where(Expr::col(Releases::Lang).eq(lang))
            .to_owned(),
    )
}

// ------------------------------------------------------------------ admin ---

pub fn translation_details(id: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([
                (X, Translations::Id),
                (X, Translations::TextKey),
                (X, Translations::Lang),
                (X, Translations::Tr),
                (X, Translations::Source),
                (X, Translations::Review),
                (X, Translations::ReviewNote),
                (X, Translations::Up),
                (X, Translations::Down),
                (X, Translations::Score),
                (X, Translations::Reporters),
                (X, Translations::CreatedAt),
                (X, Translations::PassedAt),
            ])
            .column((T, Texts::En))
            .expr_as(Expr::col((T, Texts::State)), Alias::new("text_state"))
            .from_as(Translations::Table, X)
            .join_as(
                JoinType::InnerJoin,
                Texts::Table,
                T,
                Expr::col((T, Texts::Key)).equals((X, Translations::TextKey)),
            )
            .and_where(Expr::col((X, Translations::Id)).eq(id))
            .to_owned(),
    )
}

pub fn translation_reporters(id: &str) -> Q {
    sqlite(
        &Query::select()
            .column(TranslationReporters::IpHash)
            .from(TranslationReporters::Table)
            .and_where(Expr::col(TranslationReporters::TranslationId).eq(id))
            .to_owned(),
    )
}

pub fn translation_votes(id: &str) -> Q {
    sqlite(
        &Query::select()
            .columns([Votes::IpHash, Votes::Value, Votes::CreatedAt])
            .from(Votes::Table)
            .and_where(Expr::col(Votes::TranslationId).eq(id))
            .order_by(Votes::CreatedAt, Order::Asc)
            .to_owned(),
    )
}

pub fn hide_translation(id: &str, note: &str) -> Q {
    sqlite(
        &Query::update()
            .table(Translations::Table)
            .values([
                (Translations::Review, "hidden".into()),
                (Translations::ReviewNote, note.into()),
            ])
            .and_where(Expr::col(Translations::Id).eq(id))
            .to_owned(),
    )
}

/// Back to `passed` if it had passed before, otherwise back to review.
pub fn unhide_translation(id: &str, now: &str) -> Vec<Q> {
    vec![
        sqlite(
            &Query::update()
                .table(Translations::Table)
                .value(
                    Translations::Review,
                    Expr::case(Expr::col(Translations::PassedAt).is_null(), "pending").finally("passed"),
                )
                .value(Translations::ReviewNote, "admin: unhidden")
                .and_where(Expr::col(Translations::Id).eq(id))
                .and_where(Expr::col(Translations::Review).eq("hidden"))
                .to_owned(),
        ),
        enqueue_translation_if_pending(id, now),
    ]
}

/// Votes cast by an IP hash, with their translation's text and language.
pub fn votes_by_ip(ip_hash: &str) -> Q {
    sqlite(
        &Query::select()
            .column((V, Votes::TranslationId))
            .column((X, Translations::TextKey))
            .column((X, Translations::Lang))
            .from_as(Votes::Table, V)
            .join_as(
                JoinType::InnerJoin,
                Translations::Table,
                X,
                Expr::col((X, Translations::Id)).equals((V, Votes::TranslationId)),
            )
            .and_where(Expr::col((V, Votes::IpHash)).eq(ip_hash))
            .to_owned(),
    )
}

pub fn delete_votes_by_ip(ip_hash: &str) -> Q {
    sqlite(
        &Query::delete()
            .from_table(Votes::Table)
            .and_where(Expr::col(Votes::IpHash).eq(ip_hash))
            .to_owned(),
    )
}

/// Hides pending site corrections sent only by `ip_hash`.
pub fn hide_corrections_only_by(ip_hash: &str) -> Q {
    let by_ip = Query::select()
        .column(TranslationReporters::TranslationId)
        .from(TranslationReporters::Table)
        .and_where(Expr::col(TranslationReporters::IpHash).eq(ip_hash))
        .to_owned();
    let others = Query::select()
        .expr(Expr::val(1))
        .from_as(TranslationReporters::Table, R)
        .and_where(Expr::col((R, TranslationReporters::TranslationId)).equals((Translations::Table, Translations::Id)))
        .and_where(Expr::col((R, TranslationReporters::IpHash)).ne(ip_hash))
        .to_owned();
    sqlite(
        &Query::update()
            .table(Translations::Table)
            .values([
                (Translations::Review, "hidden".into()),
                (Translations::ReviewNote, "admin: banned reporter".into()),
            ])
            .and_where(Expr::col(Translations::Review).eq("pending"))
            .and_where(Expr::col(Translations::Source).eq("user"))
            .and_where(Expr::col(Translations::Id).in_subquery(by_ip))
            .and_where(Expr::not_exists(others))
            .to_owned(),
    )
}

// ----------------------------------------------------------------- backup ---

/// Tables exported by the backup (all but the rate-limit buckets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupTable {
    Texts,
    TextReporters,
    IdPairs,
    IdPairReporters,
    Translations,
    TranslationReporters,
    Current,
    Votes,
    ReviewQueue,
    Releases,
    Counters,
    Bans,
    Kv,
}

impl BackupTable {
    pub const ALL: [BackupTable; 13] = [
        BackupTable::Texts,
        BackupTable::TextReporters,
        BackupTable::IdPairs,
        BackupTable::IdPairReporters,
        BackupTable::Translations,
        BackupTable::TranslationReporters,
        BackupTable::Current,
        BackupTable::Votes,
        BackupTable::ReviewQueue,
        BackupTable::Releases,
        BackupTable::Counters,
        BackupTable::Bans,
        BackupTable::Kv,
    ];

    fn iden(self) -> sea_query::DynIden {
        use sea_query::IntoIden;
        match self {
            BackupTable::Texts => Texts::Table.into_iden(),
            BackupTable::TextReporters => TextReporters::Table.into_iden(),
            BackupTable::IdPairs => IdPairs::Table.into_iden(),
            BackupTable::IdPairReporters => IdPairReporters::Table.into_iden(),
            BackupTable::Translations => Translations::Table.into_iden(),
            BackupTable::TranslationReporters => TranslationReporters::Table.into_iden(),
            BackupTable::Current => Current::Table.into_iden(),
            BackupTable::Votes => Votes::Table.into_iden(),
            BackupTable::ReviewQueue => ReviewQueue::Table.into_iden(),
            BackupTable::Releases => Releases::Table.into_iden(),
            BackupTable::Counters => Counters::Table.into_iden(),
            BackupTable::Bans => Bans::Table.into_iden(),
            BackupTable::Kv => Kv::Table.into_iden(),
        }
    }

    pub fn name(self) -> String {
        self.iden().to_string()
    }
}

/// `SELECT rowid AS _rowid, * FROM <table> WHERE rowid > ? ORDER BY rowid LIMIT ?`.
pub fn backup_page(table: BackupTable, after_rowid: i64, limit: u64) -> Q {
    sqlite(
        &Query::select()
            .expr_as(Expr::col(RowId), Alias::new("_rowid"))
            .column(sea_query::Asterisk)
            .from(table.iden())
            .and_where(Expr::col(RowId).gt(after_rowid))
            .order_by(RowId, Order::Asc)
            .limit(limit)
            .to_owned(),
    )
}

#[cfg(test)]
mod tests;
