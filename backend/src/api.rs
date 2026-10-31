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
    for raw in req.items {
        let Ok(item) = serde_json::from_value::<RawItem>(raw) else {
            out.rejected += 1;
            continue;
        };
        match prepare_item(item, &req.lang) {
            Ok((p, tr_ok)) => {
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
    Recent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextsQuery {
    pub lang: String,
    pub sort: Sort,
    /// Search text (English or translation), at most 100 chars.
    pub q: Option<String>,
    /// Opaque cursor: the offset of the next page.
    pub offset: u32,
    pub limit: u32,
}

pub const PAGE_SIZE: u32 = 30;
/// Deepest page served (keeps OFFSET scans cheap).
pub const MAX_OFFSET: u32 = 3000;

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
    for (k, v) in pairs {
        match k {
            "lang" => q.lang = v.to_owned(),
            "sort" => {
                q.sort = match v {
                    "worst" => Sort::Worst,
                    "best" => Sort::Best,
                    "recent" | "" => Sort::Recent,
                    _ => return Err(ApiError::bad("bad_sort")),
                }
            }
            "q" => {
                let t = v.trim();
                if !t.is_empty() {
                    q.q = Some(t.chars().take(100).collect());
                }
            }
            "cursor" if !v.is_empty() => {
                q.offset = v.parse().map_err(|_| ApiError::bad("bad_cursor"))?;
                if q.offset > MAX_OFFSET {
                    return Err(ApiError::bad("bad_cursor"));
                }
            }
            _ => {}
        }
    }
    if !validate::lang_allowed(&q.lang, allowed_langs) {
        return Err(ApiError::bad("unsupported_lang"));
    }
    Ok(q)
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
        assert!(parse_texts_query([("lang", "pt-BR"), ("cursor", "999999")], LANGS).is_err());
        assert!(parse_texts_query([("sort", "best")], LANGS).is_err());
        assert_eq!(like_pattern("50%_a\\"), "%50\\%\\_a\\\\%");
    }
}
