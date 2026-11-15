//! Request bodies and their validation (pure; the Worker glue only does I/O).

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::Value;

use crate::hash::{is_key, text_key, translation_id};
use crate::validate::{self, Reject};

/// Most items in one contribution request.
pub const MAX_ITEMS: usize = 500;
/// Most bytes in one request body (after gunzip).
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// An error answered as `{"error": code}` with an HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
}

impl ApiError {
    pub fn new(status: u16, code: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
        }
    }
    pub fn bad(code: impl Into<String>) -> Self {
        Self::new(400, code)
    }
}

#[derive(Debug, Deserialize)]
pub struct ContributionRequest {
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub game_build: Option<u64>,
    pub lang: String,
    pub items: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    #[serde(default)]
    id: Option<u64>,
    en: String,
    #[serde(default)]
    tr: Option<String>,
    #[serde(default)]
    src: Option<String>,
}

/// A translation that passed the rule checks, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedTr {
    pub id: String,
    pub tr: String,
    pub source: &'static str,
}

/// An item that passed the rule checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedItem {
    pub text_key: String,
    pub en: String,
    pub game_id: Option<u32>,
    pub tr: Option<PreparedTr>,
}

#[derive(Debug, Default)]
pub struct Prepared {
    pub lang: String,
    pub game_build: Option<u64>,
    pub items: Vec<PreparedItem>,
    /// Items in the request.
    pub total: usize,
    /// Items refused (malformed, failing checks, duplicates in the request).
    /// An item whose English is fine but whose translation is not keeps the
    /// English/ID part and still counts as rejected.
    pub rejected: usize,
}

/// Parses and checks a contribution body (already size-checked and gunzipped).
pub fn prepare_contribution(body: &[u8], allowed_langs: &str) -> Result<Prepared, ApiError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(ApiError::new(413, "body_too_large"));
    }
    let req: ContributionRequest = serde_json::from_slice(body).map_err(|_| ApiError::bad("malformed_body"))?;
    if req.items.len() > MAX_ITEMS {
        return Err(ApiError::new(413, "too_many_items"));
    }
    if !validate::lang_allowed(&req.lang, allowed_langs) {
        return Err(ApiError::bad("unsupported_lang"));
    }
    let mut out = Prepared {
        lang: req.lang.clone(),
        game_build: req.game_build,
        total: req.items.len(),
        ..Default::default()
    };
    let mut seen = HashSet::new();
    // One translation per text per request: the per-text cap is checked against stored rows, so several variants of
    // one text in a single request would all pass it.
    let mut translated = HashSet::new();
    for raw in req.items {
        let Ok(item) = serde_json::from_value::<RawItem>(raw) else {
            out.rejected += 1;
            continue;
        };
        match prepare_item(item, &req.lang) {
            Ok((mut p, mut tr_ok)) => {
                if p.tr.is_some() && !translated.insert(p.text_key.clone()) {
                    p.tr = None;
                    tr_ok = false;
                }
                let dedupe = (p.text_key.clone(), p.game_id, p.tr.as_ref().map(|t| t.id.clone()));
                if !seen.insert(dedupe) {
                    out.rejected += 1;
                    continue;
                }
                if !tr_ok {
                    out.rejected += 1;
                }
                out.items.push(p);
            }
            Err(_) => out.rejected += 1,
        }
    }
    Ok(out)
}

impl Prepared {
    /// `accepted` of the 202 response (`accepted + rejected == items sent`).
    pub fn accepted(&self) -> usize {
        self.total - self.rejected
    }
}

/// Returns the item and whether its translation (if any) was kept.
fn prepare_item(item: RawItem, lang: &str) -> Result<(PreparedItem, bool), Reject> {
    validate::check_en(&item.en)?;
    let game_id = match item.id {
        None => None,
        Some(id) if id > 0 && id <= u32::MAX as u64 => Some(id as u32),
        Some(_) => return Err(Reject::Id),
    };
    let key = text_key(&item.en);
    let (tr, ok) = match item.tr {
        None => (None, true),
        Some(tr) => match validate::check_tr(&item.en, &tr).and_then(|_| validate::source(item.src.as_deref())) {
            Ok(source) => (
                Some(PreparedTr {
                    id: translation_id(&key, lang, &tr),
                    tr,
                    source,
                }),
                true,
            ),
            Err(_) => (None, false),
        },
    };
    Ok((
        PreparedItem {
            text_key: key,
            en: item.en,
            game_id,
            tr,
        },
        ok,
    ))
}

#[derive(Debug, Deserialize)]
pub struct VoteRequest {
    pub translation_id: String,
    pub value: i64,
    #[serde(default)]
    pub turnstile: String,
}

impl VoteRequest {
    pub fn parse(body: &[u8]) -> Result<Self, ApiError> {
        let v: Self = serde_json::from_slice(body).map_err(|_| ApiError::bad("malformed_body"))?;
        if !is_key(&v.translation_id) {
            return Err(ApiError::bad("bad_translation_id"));
        }
        if v.value != 1 && v.value != -1 {
            return Err(ApiError::bad("bad_value"));
        }
        if v.turnstile.is_empty() || v.turnstile.len() > 4096 {
            return Err(ApiError::new(403, "turnstile_required"));
        }
        Ok(v)
    }
}

#[derive(Debug, Deserialize)]
pub struct CorrectionRequest {
    pub text_key: String,
    pub lang: String,
    pub tr: String,
    #[serde(default)]
    pub turnstile: String,
}

impl CorrectionRequest {
    pub fn parse(body: &[u8], allowed_langs: &str) -> Result<Self, ApiError> {
        let v: Self = serde_json::from_slice(body).map_err(|_| ApiError::bad("malformed_body"))?;
        if !is_key(&v.text_key) {
            return Err(ApiError::bad("bad_text_key"));
        }
        if !validate::lang_allowed(&v.lang, allowed_langs) {
            return Err(ApiError::bad("unsupported_lang"));
        }
        if v.turnstile.is_empty() || v.turnstile.len() > 4096 {
            return Err(ApiError::new(403, "turnstile_required"));
        }
        Ok(v)
    }
}

/// `sort` of `GET /v1/texts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Worst,
    Best,
    /// Newest current translation first.
    Recent,
    /// Oldest current translation first.
    Oldest,
}

impl Sort {
    pub fn name(self) -> &'static str {
        match self {
            Sort::Worst => "worst",
            Sort::Best => "best",
            Sort::Recent => "recent",
            Sort::Oldest => "oldest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextsQuery {
    pub lang: String,
    pub sort: Sort,
    /// Search text (English or translation), at most 100 chars.
    pub q: Option<String>,
    /// Rows skipped: `(page - 1) * limit`, or the old numeric `cursor`.
    pub offset: u32,
    /// Rows per page, 1 to [`MAX_PAGE_SIZE`].
    pub limit: u32,
}

/// Default page size.
pub const PAGE_SIZE: u32 = 20;
/// Largest `limit`.
pub const MAX_PAGE_SIZE: u32 = 100;
/// Deepest row of the plain listing (more than every text the game has; the ordering is index-backed).
pub const MAX_OFFSET: u32 = 2_000_000;
/// Deepest row of a search: it scans for matches, so only its first pages are served.
pub const MAX_SEARCH_OFFSET: u32 = 3000;

/// Parses the query string pairs of `GET /v1/texts`.
pub fn parse_texts_query<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    allowed_langs: &str,
) -> Result<TextsQuery, ApiError> {
    let mut q = TextsQuery {
        lang: String::new(),
        sort: Sort::Recent,
        q: None,
        offset: 0,
        limit: PAGE_SIZE,
    };
    let (mut page, mut cursor) = (None, None);
    for (k, v) in pairs {
        match k {
            "lang" => q.lang = v.to_owned(),
            "sort" => {
                q.sort = match v {
                    "worst" => Sort::Worst,
                    "best" => Sort::Best,
                    "recent" | "" => Sort::Recent,
                    "oldest" => Sort::Oldest,
                    _ => return Err(ApiError::bad("bad_sort")),
                }
            }
            "q" => {
                let t = v.trim();
                if !t.is_empty() {
                    q.q = Some(t.chars().take(100).collect());
                }
            }
            "cursor" if !v.is_empty() => cursor = Some(small_number(v).ok_or_else(|| ApiError::bad("bad_cursor"))?),
            "page" if !v.is_empty() => page = Some(small_number(v).filter(|p| *p >= 1).ok_or_else(|| ApiError::bad("bad_page"))?),
            "limit" if !v.is_empty() => {
                q.limit = small_number(v)
                    .filter(|n| (1..=MAX_PAGE_SIZE).contains(n))
                    .ok_or_else(|| ApiError::bad("bad_limit"))?
            }
            _ => {}
        }
    }
    if !validate::lang_allowed(&q.lang, allowed_langs) {
        return Err(ApiError::bad("unsupported_lang"));
    }
    // `page` wins over the old numeric `cursor` (an offset).
    let offset = match (page, cursor) {
        (Some(p), _) => (p as u64 - 1) * q.limit as u64,
        (None, Some(c)) => c as u64,
        (None, None) => 0,
    };
    let max = if q.q.is_some() { MAX_SEARCH_OFFSET } else { MAX_OFFSET };
    if offset > max as u64 {
        return Err(ApiError::bad(if page.is_some() { "bad_page" } else { "bad_cursor" }));
    }
    q.offset = offset as u32;
    Ok(q)
}

/// A plain decimal number of at most 7 digits, without sign or leading zeros.
fn small_number(v: &str) -> Option<u32> {
    if v.is_empty() || v.len() > 7 || !v.bytes().all(|b| b.is_ascii_digit()) || (v.len() > 1 && v.starts_with('0')) {
        return None;
    }
    v.parse().ok()
}

/// Edge-cache key of a `GET /v1/texts` answer: the canonical URL of the page (so extra or reordered parameters
/// cannot multiply cache entries). Searches are cached too (the search text percent-encoded): each one scans the
/// whole dictionary, so a repeated search must not.
pub fn texts_cache_key(origin: &str, q: &TextsQuery) -> Option<String> {
    let mut key = format!(
        "{}/v1/texts?lang={}&sort={}&offset={}&limit={}",
        origin.trim_end_matches('/'),
        q.lang,
        q.sort.name(),
        q.offset,
        q.limit
    );
    if let Some(s) = &q.q {
        key.push_str("&q=");
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                key.push(b as char);
            } else {
                key.push_str(&format!("%{b:02X}"));
            }
        }
    }
    Some(key)
}

/// Escapes `%`, `_` and `\` for `LIKE ... ESCAPE '\'`.
pub fn like_pattern(q: &str) -> String {
    let mut s = String::with_capacity(q.len() + 2);
    s.push('%');
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            s.push('\\');
        }
        s.push(c);
    }
    s.push('%');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const LANGS: &str = "pt-BR";

    #[test]
    fn contract_example_is_accepted() {
        let body = r#"{
          "client": "gw2translated/0.1.0", "game_build": 209026, "lang": "pt-BR",
          "items": [
            { "id": 621576, "en": "Ingredient" },
            { "en": "Sand Lioness", "tr": "Leoa de Areia", "src": "argos" },
            { "id": 1601, "en": "The Cost of Victory", "tr": "O Custo da Vitória", "src": "argos" }
          ]}"#;
        let p = prepare_contribution(body.as_bytes(), LANGS).unwrap();
        assert_eq!((p.items.len(), p.rejected, p.accepted()), (3, 0, 3));
        assert_eq!(p.items[0].game_id, Some(621576));
        assert_eq!(p.items[2].tr.as_ref().unwrap().source, "argos");
    }

    #[test]
    fn bad_items_are_counted_not_fatal() {
        let body = br#"{"lang":"pt-BR","items":[
            {"en":""}, {"id":"x","en":"A"}, {"en":"Hit %num1% foe[s].","tr":"Acerte.","src":"argos"},
            {"en":"Sword","tr":"Espada","src":"hacker"}, {"en":"Sword"}, {"en":"Sword"}, {"id": 99999999999, "en":"Shield"}
        ]}"#;
        let p = prepare_contribution(body, LANGS).unwrap();
        // "Hit..." and the first "Sword" keep their English part; the later
        // plain "Sword" items are duplicates of it.
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.rejected, 7);
    }

    #[test]
    fn one_translation_per_text_per_request() {
        let body = br#"{"lang":"pt-BR","items":[
            {"en":"Sword","tr":"Espada","src":"argos"},
            {"en":"Sword","tr":"Espadinha","src":"argos"},
            {"en":"Sword","tr":"Espadona","src":"argos"}
        ]}"#;
        let p = prepare_contribution(body, LANGS).unwrap();
        // The first keeps its translation; the others lose it and their English part is a duplicate.
        assert_eq!(p.items.iter().filter(|i| i.tr.is_some()).count(), 1);
        assert_eq!(p.items[0].tr.as_ref().unwrap().tr, "Espada");
        assert_eq!(p.rejected, 2);
    }

    #[test]
    fn size_and_shape_errors() {
        assert_eq!(prepare_contribution(b"{", LANGS).unwrap_err().status, 400);
        assert_eq!(
            prepare_contribution(br#"{"lang":"xx","items":[]}"#, LANGS)
                .unwrap_err()
                .status,
            400
        );
        let many = format!(
            r#"{{"lang":"pt-BR","items":[{}]}}"#,
            vec![r#"{"en":"a"}"#; 501].join(",")
        );
        assert_eq!(prepare_contribution(many.as_bytes(), LANGS).unwrap_err().status, 413);
        assert_eq!(
            prepare_contribution(&vec![b' '; MAX_BODY_BYTES + 1], LANGS)
                .unwrap_err()
                .status,
            413
        );
    }

    #[test]
    fn vote_and_correction_parsing() {
        let id = "0123456789abcdef0123456789abcdef";
        assert!(
            VoteRequest::parse(format!(r#"{{"translation_id":"{id}","value":1,"turnstile":"t"}}"#).as_bytes()).is_ok()
        );
        assert_eq!(
            VoteRequest::parse(format!(r#"{{"translation_id":"{id}","value":2,"turnstile":"t"}}"#).as_bytes())
                .unwrap_err()
                .code,
            "bad_value"
        );
        assert_eq!(
            VoteRequest::parse(format!(r#"{{"translation_id":"{id}","value":1}}"#).as_bytes())
                .unwrap_err()
                .status,
            403
        );
        assert_eq!(
            VoteRequest::parse(br#"{"translation_id":"x","value":1,"turnstile":"t"}"#)
                .unwrap_err()
                .code,
            "bad_translation_id"
        );
        assert!(
            CorrectionRequest::parse(
                format!(r#"{{"text_key":"{id}","lang":"pt-BR","tr":"x","turnstile":"t"}}"#).as_bytes(),
                LANGS
            )
            .is_ok()
        );
    }

    #[test]
    fn texts_query() {
        let q = parse_texts_query(
            [("lang", "pt-BR"), ("sort", "worst"), ("q", " sword "), ("cursor", "30")],
            LANGS,
        )
        .unwrap();
        assert_eq!((q.sort, q.q.as_deref(), q.offset), (Sort::Worst, Some("sword"), 30));
        assert!(parse_texts_query([("lang", "pt-BR"), ("sort", "x")], LANGS).is_err());
        let err = |pairs: &[(&str, &str)]| parse_texts_query(pairs.iter().copied(), LANGS).unwrap_err().code;
        assert_eq!(err(&[("lang", "pt-BR"), ("q", "a"), ("cursor", "3001")]), "bad_cursor", "searches stop early");
        assert_eq!(err(&[("lang", "pt-BR"), ("cursor", "2000001")]), "bad_cursor");
        for bad in ["0", "-1", "x", "01", "12345678", "1.5"] {
            assert_eq!(err(&[("lang", "pt-BR"), ("page", bad)]), "bad_page", "{bad}");
        }
        for bad in ["0", "101", "-5", "020", "abc"] {
            assert_eq!(err(&[("lang", "pt-BR"), ("limit", bad)]), "bad_limit", "{bad}");
        }
        // Page 11452 of 20 is a deep but valid plain page; the same page of a search is not.
        let q = parse_texts_query([("lang", "pt-BR"), ("sort", "oldest"), ("page", "11452"), ("limit", "20")], LANGS).unwrap();
        assert_eq!((q.sort, q.offset, q.limit), (Sort::Oldest, 229_020, 20));
        assert_eq!(err(&[("lang", "pt-BR"), ("q", "a"), ("page", "200"), ("limit", "20")]), "bad_page");
        assert_eq!(err(&[("lang", "pt-BR"), ("page", "20002"), ("limit", "100")]), "bad_page");
        let q = parse_texts_query([("lang", "pt-BR"), ("page", "3"), ("cursor", "7"), ("limit", "50")], LANGS).unwrap();
        assert_eq!(q.offset, 100, "page wins over cursor");
        let q = parse_texts_query([("lang", "pt-BR")], LANGS).unwrap();
        assert_eq!((q.sort, q.offset, q.limit), (Sort::Recent, 0, PAGE_SIZE));
        assert!(parse_texts_query([("sort", "best")], LANGS).is_err());
        assert_eq!(like_pattern("50%_a\\"), "%50\\%\\_a\\\\%");
    }

    #[test]
    fn texts_cache_key_is_canonical_and_covers_searches() {
        let key = |pairs: &[(&str, &str)]| {
            texts_cache_key("https://x.dev/", &parse_texts_query(pairs.iter().copied(), LANGS).unwrap())
        };
        let a = key(&[("lang", "pt-BR"), ("sort", "best"), ("page", "3"), ("limit", "50")]);
        assert_eq!(a.as_deref(), Some("https://x.dev/v1/texts?lang=pt-BR&sort=best&offset=100&limit=50"));
        // Same page asked another way, or with junk parameters: same entry.
        assert_eq!(key(&[("limit", "50"), ("junk", "1"), ("cursor", "100"), ("sort", "best"), ("lang", "pt-BR")]), a);
        assert_ne!(key(&[("lang", "pt-BR"), ("sort", "worst"), ("page", "3"), ("limit", "50")]), a);
        assert_eq!(
            key(&[("lang", "pt-BR")]).as_deref(),
            Some("https://x.dev/v1/texts?lang=pt-BR&sort=recent&offset=0&limit=20")
        );
        assert_eq!(
            key(&[("lang", "pt-BR"), ("q", " sword ")]).as_deref(),
            Some("https://x.dev/v1/texts?lang=pt-BR&sort=recent&offset=0&limit=20&q=sword")
        );
        // Anything that could break out of the query string is encoded.
        assert_eq!(
            key(&[("lang", "pt-BR"), ("q", "a&b=c#d é")]).as_deref(),
            Some("https://x.dev/v1/texts?lang=pt-BR&sort=recent&offset=0&limit=20&q=a%26b%3Dc%23d%20%C3%A9")
        );
    }
}
