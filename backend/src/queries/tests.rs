//! Runs every built statement against the real migration in SQLite.

use rusqlite::Connection;
use rusqlite::types::Value as Sql;
use sea_query::Value;

use super::*;

fn db() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch(include_str!("../../migrations/0001_init.sql")).unwrap();
    c.execute_batch(include_str!("../../migrations/0002_list_indexes.sql")).unwrap();
    c
}

fn params(v: &Values) -> Vec<Sql> {
    v.0.iter()
        .map(|v| match v {
            Value::Bool(b) => b.map(|b| Sql::Integer(b as i64)).unwrap_or(Sql::Null),
            Value::TinyInt(x) => x.map(|x| Sql::Integer(x as i64)).unwrap_or(Sql::Null),
            Value::SmallInt(x) => x.map(|x| Sql::Integer(x as i64)).unwrap_or(Sql::Null),
            Value::Int(x) => x.map(|x| Sql::Integer(x as i64)).unwrap_or(Sql::Null),
            Value::BigInt(x) => x.map(Sql::Integer).unwrap_or(Sql::Null),
            Value::TinyUnsigned(x) => x.map(|x| Sql::Integer(x as i64)).unwrap_or(Sql::Null),
            Value::SmallUnsigned(x) => x.map(|x| Sql::Integer(x as i64)).unwrap_or(Sql::Null),
            Value::Unsigned(x) => x.map(|x| Sql::Integer(x as i64)).unwrap_or(Sql::Null),
            Value::BigUnsigned(x) => x.map(|x| Sql::Integer(x as i64)).unwrap_or(Sql::Null),
            Value::Float(x) => x.map(|x| Sql::Real(x as f64)).unwrap_or(Sql::Null),
            Value::Double(x) => x.map(Sql::Real).unwrap_or(Sql::Null),
            Value::String(x) => x.clone().map(Sql::Text).unwrap_or(Sql::Null),
            Value::Char(x) => x.map(|x| Sql::Text(x.to_string())).unwrap_or(Sql::Null),
            other => panic!("unexpected value {other:?}"),
        })
        .collect()
}

fn exec(c: &Connection, (sql, v): &Q) -> usize {
    c.execute(sql, rusqlite::params_from_iter(params(v)))
        .unwrap_or_else(|e| panic!("{e}\n{sql}"))
}

fn rows(c: &Connection, (sql, v): &Q) -> Vec<Vec<Sql>> {
    let mut st = c.prepare(sql).unwrap_or_else(|e| panic!("{e}\n{sql}"));
    let n = st.column_count();
    st.query_map(rusqlite::params_from_iter(params(v)), |r| {
        (0..n).map(|i| r.get::<_, Sql>(i)).collect()
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn text(v: &Sql) -> &str {
    match v {
        Sql::Text(s) => s,
        other => panic!("not text: {other:?}"),
    }
}

fn int(v: &Sql) -> i64 {
    match v {
        Sql::Integer(i) => *i,
        other => panic!("not int: {other:?}"),
    }
}

const K1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const K2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const T1: &str = "cccccccccccccccccccccccccccccccc";
const EVIL: &str = "x'); DROP TABLE texts; --";

fn contribute(c: &Connection, ip: &str) {
    let texts = serde_json::json!([{"k": K1, "en": "Sword"}, {"k": K2, "en": EVIL}]).to_string();
    let pairs = serde_json::json!([{"g": 1601, "k": K1}]).to_string();
    let trs = serde_json::json!([{"id": T1, "k": K1, "tr": "Espada", "src": "argos"}]).to_string();
    let rows = ContributionRows {
        texts: &texts,
        pairs: &pairs,
        trs: &trs,
        ip_hash: ip,
        lang: "pt-BR",
        now: "2026-01-01T00:00:00Z",
        game_build: Some(209026),
    };
    for q in contribution(&rows) {
        assert!(
            !q.0.contains("DROP") && !q.0.contains("Espada") && !q.0.contains(ip),
            "data leaked into SQL: {}",
            q.0
        );
        exec(c, &q);
    }
}

#[test]
fn contribution_stores_rows_counts_reporters_and_queues() {
    let c = db();
    contribute(&c, "ip1");
    contribute(&c, "ip2");
    let t = rows(&c, &texts_by_keys(&serde_json::json!([K1, K2]).to_string()));
    assert_eq!(t.len(), 2);
    assert!(t.iter().any(|r| text(&r[1]) == EVIL));
    assert!(t.iter().all(|r| int(&r[3]) == 2), "distinct reporters");
    let q = rows(&c, &queue_due(i64::MAX, 100));
    let kinds: Vec<&str> = q.iter().map(|r| text(&r[1])).collect();
    assert_eq!(kinds.iter().filter(|k| **k == "text").count(), 2);
    assert!(kinds.contains(&"pair") && kinds.contains(&"translation"));
    assert!(q.iter().any(|r| text(&r[2]) == format!("1601:{K1}")));
    let p = rows(&c, &pairs_for_ids("[1601]"));
    assert_eq!(
        (int(&p[0][0]), int(&p[0][2]), text(&p[0][3]), text(&p[0][4])),
        (1601, 2, "pending", "pending")
    );
    // The texts table survived the hostile English text.
    assert_eq!(rows(&c, &stats("pt-BR", 5, 0.8))[0].len(), 4);
}

#[test]
fn conflicting_pair_is_rejected_on_insert() {
    let c = db();
    contribute(&c, "ip1");
    exec(&c, &set_pair_state(1601, K1, "trusted", "now"));
    let pairs = serde_json::json!([{"g": 1601, "k": K2}]).to_string();
    let rows_in = ContributionRows {
        texts: "[]",
        pairs: &pairs,
        trs: "[]",
        ip_hash: "ip3",
        lang: "pt-BR",
        now: "now",
        game_build: None,
    };
    for q in contribution(&rows_in) {
        exec(&c, &q);
    }
    let p = rows(&c, &pairs_for_ids("[1601]"));
    let k2 = p.iter().find(|r| text(&r[1]) == K2).unwrap();
    assert_eq!(text(&k2[3]), "rejected");
}

#[test]
fn review_votes_current_and_listing() {
    let c = db();
    contribute(&c, "ip1");
    exec(&c, &set_text_state(K1, "trusted", "ai: ok", "t1"));
    exec(&c, &set_review(T1, "passed", "ai: ok", "t1"));
    let cand = rows(
        &c,
        &current_candidates(&serde_json::json!([{"k": K1, "l": "pt-BR"}]).to_string()),
    );
    assert_eq!(text(&cand[0][6]), "t1");
    let cur = CurrentRow {
        text_key: K1,
        lang: "pt-BR",
        translation_id: T1,
        score: 0.0,
        up: 0,
        down: 0,
        now: "t1",
    };
    exec(&c, &current_upsert(&cur));
    // Same translation again: updated_at stays.
    exec(
        &c,
        &current_upsert(&CurrentRow {
            now: "t2",
            score: 0.2,
            ..cur
        }),
    );
    assert_eq!(exec(&c, &vote_insert(T1, "ip9", 1, "t")), 1);
    assert_eq!(exec(&c, &vote_insert(T1, "ip9", -1, "t")), 0, "one vote per IP");
    exec(&c, &recount_votes(&serde_json::json!([T1]).to_string()));
    let v = rows(&c, &translations_votes(&serde_json::json!([T1]).to_string()));
    assert_eq!((int(&v[0][1]), int(&v[0][2])), (1, 0));
    exec(&c, &set_score(T1, 0.2065));
    let r = rows(&c, &vote_result(T1));
    assert_eq!((int(&r[0][0]), int(&r[0][2])), (1, 1));
    for sort in [ListSort::Worst, ListSort::Best, ListSort::Recent] {
        let l = rows(&c, &list_texts("pt-BR", sort, None, 31, 0));
        assert_eq!(l.len(), 1);
        assert_eq!(text(&l[0][2]), T1);
    }
    let found = rows(&c, &list_texts("pt-BR", ListSort::Recent, Some("%esp%"), 31, 0));
    assert_eq!(found.len(), 1);
    assert!(
        rows(&c, &list_texts("pt-BR", ListSort::Recent, Some("%\\%%"), 31, 0)).is_empty(),
        "escaped %"
    );
    let alt = rows(&c, &passed_translations("pt-BR", &serde_json::json!([K1]).to_string()));
    assert_eq!(text(&alt[0][2]), "Espada");
    exec(&c, &set_pair_state(1601, K1, "trusted", "t"));
    let ids = rows(&c, &trusted_ids(&serde_json::json!([K1]).to_string()));
    assert_eq!(text(&ids[0][1]), "1601");
    let page = rows(&c, &release_page("pt-BR", "", 5000));
    assert_eq!(
        (text(&page[0][1]), text(&page[0][2]), text(&page[0][4])),
        ("Sword", "Espada", "1601")
    );
    exec(
        &c,
        &current_upsert(&CurrentRow {
            now: "t3",
            score: 0.2065,
            up: 1,
            ..cur
        }),
    );
    let s = rows(&c, &stats("pt-BR", 1, 0.1));
    assert_eq!((int(&s[0][0]), int(&s[0][1]), int(&s[0][2])), (1, 1, 1));
    exec(&c, &current_delete(K1, "pt-BR"));
    assert!(rows(&c, &list_texts("pt-BR", ListSort::Best, None, 31, 0)).is_empty());
}

#[test]
fn corrections_admin_and_queue() {
    let c = db();
    contribute(&c, "ip1");
    let id = "dddddddddddddddddddddddddddddddd";
    for q in correction(id, K1, "pt-BR", EVIL, "ip5", "now") {
        assert!(!q.0.contains("DROP"));
        exec(&c, &q);
    }
    assert_eq!(text(&rows(&c, &translation_target(id))[0][2]), "pending");
    assert_eq!(text(&rows(&c, &text_get(K1))[0][0]), "Sword");
    exec(&c, &hide_corrections_only_by("ip5"));
    assert_eq!(text(&rows(&c, &translation_target(id))[0][2]), "hidden");
    for q in unhide_translation(id, "now") {
        exec(&c, &q);
    }
    assert_eq!(text(&rows(&c, &translation_target(id))[0][2]), "pending");
    exec(&c, &hide_translation(id, "admin: troll"));
    let d = rows(&c, &translation_details(id));
    assert_eq!(text(&d[0][13]), "Sword");
    assert_eq!(text(&rows(&c, &translation_reporters(id))[0][0]), "ip5");
    exec(&c, &vote_insert(T1, "ip5", 1, "t"));
    assert!(rows(&c, &translation_votes(T1)).len() == 1);
    assert_eq!(rows(&c, &votes_by_ip("ip5")).len(), 1);
    exec(&c, &delete_votes_by_ip("ip5"));
    assert!(rows(&c, &votes_by_ip("ip5")).is_empty());
    // Queue lease / retry / delete.
    let due = rows(&c, &queue_due(0, 100));
    let ids: Vec<i64> = due.iter().map(|r| int(&r[0])).collect();
    exec(&c, &queue_lease(&serde_json::json!(ids).to_string(), 1000));
    assert!(rows(&c, &queue_due(0, 100)).is_empty());
    exec(&c, &queue_retry(ids[0], 0));
    assert_eq!(rows(&c, &queue_due(0, 100)).len(), 1);
    exec(&c, &queue_delete(&serde_json::json!(ids).to_string()));
    assert!(rows(&c, &queue_due(i64::MAX, 100)).is_empty());
    exec(&c, &set_text_state(K1, "trusted", "x", "t"));
    exec(&c, &enqueue_pairs_of_texts(&serde_json::json!([K1]).to_string(), "t"));
    assert_eq!(rows(&c, &queue_due(i64::MAX, 100)).len(), 1);
}

#[test]
fn bans_rates_kv_counters_releases_backup() {
    let c = db();
    exec(&c, &ban_upsert("h1", "ip", "spam", "t"));
    exec(&c, &ban_upsert("h1", "ip", "spam again", "t"));
    assert_eq!(rows(&c, &ban_check("zz", "h1")).len(), 1);
    exec(&c, &ban_delete("h1"));
    assert!(rows(&c, &ban_check("h1", "h1")).is_empty());
    exec(&c, &rate_put("vote:h", 3.5, 10));
    exec(&c, &rate_put("vote:h", 2.5, 20));
    let r = rows(&c, &rate_get("vote:h"));
    assert_eq!((&r[0][0], int(&r[0][1])), (&Sql::Real(2.5), 20));
    assert_eq!(exec(&c, &rate_cleanup(21)), 1);
    exec(&c, &kv_put("stats:pt-BR", "{}", "t"));
    exec(&c, &kv_put("stats:pt-BR", "{\"a\":1}", "t"));
    assert_eq!(text(&rows(&c, &kv_get("stats:pt-BR"))[0][0]), "{\"a\":1}");
    exec(&c, &kv_delete("stats:pt-BR"));
    exec(&c, &counter_increment("downloads"));
    assert_eq!(int(&rows(&c, &counter_get("downloads"))[0][0]), 1);
    assert_eq!(rows(&c, &max_version("pt-BR"))[0][0], Sql::Null);
    for v in 1..=2 {
        let r = NewRelease {
            lang: "pt-BR",
            version: v,
            created_at: "t",
            files: "[]",
            manifest: "{\"m\":1}",
            content_hash: "h",
            records: 0,
        };
        for q in release_insert(&r) {
            exec(&c, &q);
        }
    }
    assert_eq!(int(&rows(&c, &active_release("pt-BR"))[0][0]), 2);
    exec(&c, &release_activate("pt-BR", 1));
    assert_eq!(int(&rows(&c, &active_release("pt-BR"))[0][0]), 1);
    assert_eq!(text(&rows(&c, &active_manifest("pt-BR"))[0][0]), "{\"m\":1}");
    assert_eq!(rows(&c, &release_list("pt-BR")).len(), 2);
    assert_eq!(rows(&c, &release_exists("pt-BR", 2)).len(), 1);
    assert_eq!(int(&rows(&c, &max_version("pt-BR"))[0][0]), 2);
    contribute(&c, "ip1");
    for t in BackupTable::ALL {
        let page = rows(&c, &backup_page(t, 0, 5000));
        if t == BackupTable::Texts {
            assert_eq!(page.len(), 2);
            assert_eq!(int(&page[0][0]), 1);
        }
    }
    assert_eq!(BackupTable::ReviewQueue.name(), "review_queue");
}

#[test]
fn sql_never_contains_values() {
    let (sql, values) = ban_check(EVIL, EVIL);
    assert!(!sql.contains(EVIL));
    assert_eq!(values.0.len(), 4, "{sql}");
    let (sql, _) = list_texts("pt-BR", ListSort::Worst, Some(EVIL), 31, 0);
    assert!(!sql.contains("DROP"), "{sql}");
}

fn upload(c: &Connection, ip: &str, trs: serde_json::Value) {
    let texts = serde_json::json!([{"k": K1, "en": "Sword"}]).to_string();
    let trs = trs.to_string();
    let rows = ContributionRows {
        texts: &texts,
        pairs: "[]",
        trs: &trs,
        ip_hash: ip,
        lang: "pt-BR",
        now: "2026-01-01T00:00:00Z",
        game_build: None,
    };
    for q in contribution(&rows) {
        exec(c, &q);
    }
}

fn count(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[test]
fn a_text_collects_at_most_max_translations() {
    let c = db();
    for i in 0..MAX_TRANSLATIONS_PER_TEXT + 5 {
        let id = format!("{i:032x}");
        upload(&c, &format!("ip{i}"), serde_json::json!([{"id": id, "k": K1, "tr": format!("Espada {i}"), "src": "argos"}]));
    }
    assert_eq!(count(&c, "SELECT count(*) FROM translations"), MAX_TRANSLATIONS_PER_TEXT);
    // A refused one leaves no reporter row; an existing one still gains reporters.
    assert_eq!(count(&c, "SELECT count(DISTINCT translation_id) FROM translation_reporters"), MAX_TRANSLATIONS_PER_TEXT);
    upload(&c, "late", serde_json::json!([{"id": format!("{:032x}", 0), "k": K1, "tr": "Espada 0", "src": "argos"}]));
    assert_eq!(count(&c, "SELECT reporters FROM translations WHERE tr = 'Espada 0'"), 2);
    // A hidden translation frees its room.
    c.execute_batch("UPDATE translations SET review = 'hidden' WHERE tr = 'Espada 1'").unwrap();
    upload(&c, "new", serde_json::json!([{"id": format!("{:032x}", 99), "k": K1, "tr": "Espada nova", "src": "argos"}]));
    assert_eq!(count(&c, "SELECT count(*) FROM translations WHERE tr = 'Espada nova'"), 1);
}

#[test]
fn no_uploads_for_a_text_the_server_ai_translated() {
    let c = db();
    c.execute_batch(&format!(
        "INSERT INTO texts (key, en, created_at) VALUES ('{K1}', 'Sword', 'now');
         INSERT INTO translations (id, text_key, lang, tr, source, review, created_at)
         VALUES ('{T1}', '{K1}', 'pt-BR', 'Espada', 'ai', 'passed', 'now');"
    ))
    .unwrap();
    upload(&c, "ip1", serde_json::json!([{"id": K2, "k": K1, "tr": "Espadinha", "src": "argos"}]));
    assert_eq!(count(&c, "SELECT count(*) FROM translations"), 1);
    assert_eq!(count(&c, "SELECT count(*) FROM translation_reporters"), 0);
    // Another language is not affected.
    let texts = serde_json::json!([{"k": K1, "en": "Sword"}]).to_string();
    let trs = serde_json::json!([{"id": K2, "k": K1, "tr": "Espada", "src": "argos"}]).to_string();
    let rows = ContributionRows { texts: &texts, pairs: "[]", trs: &trs, ip_hash: "ip2", lang: "es", now: "now", game_build: None };
    for q in contribution(&rows) {
        exec(&c, &q);
    }
    assert_eq!(count(&c, "SELECT count(*) FROM translations WHERE lang = 'es'"), 1);
}

/// (text key, score, up, down, updated_at) of listed texts, with ties on every sort key.
const LISTED: [(&str, f64, i64, i64, &str); 7] = [
    ("00000000000000000000000000000001", 0.5, 3, 1, "2026-01-02T00:00:00Z"),
    ("00000000000000000000000000000002", 0.5, 3, 1, "2026-01-01T00:00:00Z"),
    ("00000000000000000000000000000003", 0.0, 0, 0, "2026-01-02T00:00:00Z"),
    ("00000000000000000000000000000004", 0.0, 0, 6, "2026-01-03T00:00:00Z"),
    ("00000000000000000000000000000005", 0.0, 0, 0, "2026-01-01T00:00:00Z"),
    ("00000000000000000000000000000006", 0.9, 9, 0, "2026-01-02T00:00:00Z"),
    ("00000000000000000000000000000007", 0.2, 1, 1, "2026-01-04T00:00:00Z"),
];

fn listing_db() -> Connection {
    let c = db();
    let mut all: Vec<(&str, f64, i64, i64, &str, &str, &str)> =
        LISTED.iter().map(|&(k, s, u, d, t)| (k, s, u, d, t, "trusted", "pt-BR")).collect();
    // Never listed: an untrusted text, and a translation in another language.
    all.push(("00000000000000000000000000000008", 1.0, 50, 0, "2026-01-09T00:00:00Z", "pending", "pt-BR"));
    all.push(("00000000000000000000000000000009", 1.0, 50, 0, "2026-01-09T00:00:00Z", "trusted", "es"));
    for (k, score, up, down, at, state, lang) in all {
        c.execute(
            "INSERT INTO texts (key, en, state, created_at) VALUES (?1, 'en ' || ?1, ?2, 'c')",
            rusqlite::params![k, state],
        )
        .unwrap();
        exec(
            &c,
            &current_upsert(&CurrentRow { text_key: k, lang, translation_id: k, score, up, down, now: at }),
        );
    }
    c
}

fn listed_keys(c: &Connection, sort: ListSort, limit: u64, offset: u64) -> Vec<String> {
    rows(c, &list_texts("pt-BR", sort, None, limit, offset))
        .iter()
        .map(|r| text(&r[0]).to_owned())
        .collect()
}

#[test]
fn listing_orders_by_sort_key_then_text_key_and_pages_by_offset() {
    let c = listing_db();
    let expect = |sort: ListSort| -> Vec<String> {
        let mut v = LISTED.to_vec();
        v.sort_by(|a, b| {
            let o = match sort {
                ListSort::Recent => b.4.cmp(a.4).then(b.0.cmp(a.0)),
                ListSort::Oldest => a.4.cmp(b.4).then(a.0.cmp(b.0)),
                ListSort::Best => b.1.partial_cmp(&a.1).unwrap().then(b.0.cmp(a.0)),
                ListSort::Worst => (b.3 - b.2).cmp(&(a.3 - a.2)).then(b.0.cmp(a.0)),
            };
            o
        });
        v.iter().map(|r| r.0.to_owned()).collect()
    };
    for sort in [ListSort::Recent, ListSort::Oldest, ListSort::Best, ListSort::Worst] {
        let all = listed_keys(&c, sort, 100, 0);
        assert_eq!(all, expect(sort), "{sort:?}");
        // Pages of 3 put back together give the whole list; past the end is empty.
        let mut paged = Vec::new();
        for p in 0..3 {
            let page = listed_keys(&c, sort, 3, p * 3);
            assert_eq!(page.len(), if p < 2 { 3 } else { 1 }, "{sort:?} page {p}");
            paged.extend(page);
        }
        assert_eq!(paged, all, "{sort:?}");
        assert!(listed_keys(&c, sort, 3, 7).is_empty());
        assert!(listed_keys(&c, sort, 3, 2_000_000).is_empty());
    }
    // The examples the tie-breaks are about.
    let best = listed_keys(&c, ListSort::Best, 3, 0);
    assert_eq!(best[1..], [LISTED[1].0.to_owned(), LISTED[0].0.to_owned()], "equal scores: text key");
    assert_eq!(listed_keys(&c, ListSort::Worst, 1, 0), [LISTED[3].0.to_owned()], "most down votes first");
    assert_eq!(listed_keys(&c, ListSort::Oldest, 2, 0), [LISTED[1].0.to_owned(), LISTED[4].0.to_owned()]);
}

#[test]
fn listing_reads_an_index_in_order_without_sorting() {
    let c = listing_db();
    for (sort, index) in [
        (ListSort::Recent, "current_updated"),
        (ListSort::Oldest, "current_updated"),
        (ListSort::Best, "current_score"),
        (ListSort::Worst, "current_net"),
    ] {
        let (sql, v) = list_texts("pt-BR", sort, None, 20, 4000);
        assert!(sql.contains("FROM \"current\" AS \"c\" CROSS JOIN \"texts\" AS \"t\" WHERE"), "{sql}");
        let plan: Vec<String> = rows(&c, &(format!("EXPLAIN QUERY PLAN {sql}"), v))
            .iter()
            .map(|r| text(&r[3]).to_owned())
            .collect();
        let plan = plan.join(" | ");
        assert!(plan.contains(index), "{sort:?}: {plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{sort:?} sorts: {plan}");
    }
}

#[test]
fn release_pages_read_current_in_key_order_without_sorting() {
    let c = listing_db();
    let p = plan(&c, &release_page("pt-BR", "k", 5000));
    assert!(p.starts_with("SEARCH c USING INDEX sqlite_autoindex_current_1 (text_key>?)"), "{p}");
    assert!(!p.contains("TEMP B-TREE"), "sorts: {p}");
}

fn plan(c: &Connection, (sql, v): &Q) -> String {
    rows(c, &(format!("EXPLAIN QUERY PLAN {sql}"), v.clone()))
        .iter()
        .map(|r| text(&r[3]).to_owned())
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn contribution_rows_stay_within_the_budget_estimate() {
    // Table rows SQLite changes (D1 also bills one row per index entry: the estimate counts those, so the table rows
    // alone must be well under it).
    let c = db();
    let before = c.total_changes();
    contribute(&c, "ip1"); // 2 new texts, 1 pair, 1 translation
    let first = c.total_changes() - before;
    let estimate = crate::budget::contribution_rows(2, 1, 1) - 3 - 2 * crate::budget::BUCKET_ROWS - crate::budget::RESERVE_ROWS;
    assert!(first <= estimate / 2, "{first} table rows vs estimate {estimate}");
    // The same items again from another player: no new texts, only reporters and recounts.
    let before = c.total_changes();
    contribute(&c, "ip2");
    let again = c.total_changes() - before;
    assert!(again < first, "{again} vs {first}");
    // And again from the same player: only the recount updates.
    let before = c.total_changes();
    contribute(&c, "ip2");
    let same = c.total_changes() - before;
    assert_eq!(same, 2 + 1 + 1, "texts, pair and translation recounts");
}

#[test]
fn budget_reserves_up_to_the_cap_and_refunds() {
    let c = db();
    let reserve = |units, cap| rows(&c, &budget_reserve("budget:w:d", units, cap));
    // A new day's row, then additions while the total stays within the cap.
    assert_eq!(int(&reserve(60, 100)[0][0]), 60);
    assert_eq!(int(&reserve(40, 100)[0][0]), 100);
    // Refused: no row back and nothing added.
    assert!(reserve(1, 100).is_empty());
    assert_eq!(int(&rows(&c, &counter_get("budget:w:d"))[0][0]), 100);
    // A refund of an over-estimate, never below 0; a charge always goes in.
    assert_eq!(int(&rows(&c, &budget_add("budget:w:d", -30))[0][0]), 70);
    assert_eq!(int(&rows(&c, &budget_add("budget:w:d", -500))[0][0]), 0);
    assert_eq!(int(&rows(&c, &budget_add("budget:w:d", 7))[0][0]), 7);
    assert_eq!(int(&rows(&c, &budget_add("budget:r:new", 9))[0][0]), 9);
    assert_eq!(int(&rows(&c, &budget_add("budget:r:new2", -9))[0][0]), 0);
    // The download counter is untouched.
    assert_eq!(int(&rows(&c, &counter_get("downloads"))[0][0]), 0);
}

#[test]
fn rate_buckets_read_together() {
    let c = db();
    exec(&c, &rate_put("vote:ip", 1.0, 10));
    exec(&c, &rate_put("vote-net:net", 2.0, 20));
    let r = rows(&c, &rate_get_many(&["vote:ip".into(), "vote-net:net".into(), "vote:other".into()]));
    assert_eq!(r.len(), 2);
    assert!(plan(&c, &rate_get_many(&["a".into(), "b".into()])).contains("USING INDEX"));
}

#[test]
fn review_queue_reads_only_the_leased_rows() {
    let c = db();
    // Due rows come out in index order (new items first), so no sort of the whole queue.
    let p = plan(&c, &queue_due(1_000, 200));
    assert!(p.contains("review_queue_next"), "{p}");
    assert!(!p.contains("TEMP B-TREE"), "{p}");
    for (i, nb) in [(1, 500), (2, 0), (3, 0), (4, 2_000)] {
        exec(
            &c,
            &(
                "INSERT INTO review_queue (id, kind, ref, enqueued_at, not_before_ms) VALUES (?, 'text', ?, 't', ?)".into(),
                Values(vec![Value::BigInt(Some(i)), Value::String(Some(format!("r{i}"))), Value::BigInt(Some(nb))]),
            ),
        );
    }
    let ids: Vec<i64> = rows(&c, &queue_due(1_000, 10)).iter().map(|r| int(&r[0])).collect();
    assert_eq!(ids, [2, 3, 1]);
}

#[test]
fn point_lookups_of_write_paths_use_indexes() {
    let c = db();
    for q in [
        ban_check("a", "b"),
        rate_get("x"),
        counter_get("downloads"),
        translation_target("t"),
        text_get("k"),
        release_exists("pt-BR", 1),
        active_manifest("pt-BR"),
        kv_get("stats:pt-BR"),
    ] {
        let p = plan(&c, &q);
        assert!(!p.starts_with("SCAN") && !p.contains("| SCAN"), "{}: {p}", q.0);
    }
}
