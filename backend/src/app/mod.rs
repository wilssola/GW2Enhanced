//! Worker glue: routing, D1/R2 access, outbound calls. Only built for wasm32.

mod admin;
mod db;
mod external;
mod jobs;
mod public;

use serde::Serialize;
use worker::wasm_bindgen::JsValue;
use worker::*;

use crate::api::ApiError;
use crate::hash::{IpHashes, ip_hashes};

/// Error of a handler: an API error answered as JSON, or an internal failure.
pub(crate) enum Fail {
    Api(ApiError),
    Internal(Error),
    /// A ready response (429 with `Retry-After`).
    Response(Response),
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Fail::Api(e)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Internal(e)
    }
}

impl From<serde_json::Error> for Fail {
    fn from(e: serde_json::Error) -> Self {
        Fail::Internal(e.into())
    }
}

pub(crate) type HResult = std::result::Result<Response, Fail>;

pub(crate) fn api_err(status: u16, code: &str) -> Fail {
    Fail::Api(ApiError::new(status, code))
}

/// Per-request context.
pub(crate) struct Ctx {
    pub env: Env,
    pub db: D1Database,
    pub now_ms: i64,
}

impl Ctx {
    pub fn new(env: Env) -> Result<Self> {
        let db = env.d1("DB")?;
        Ok(Self {
            env,
            db,
            now_ms: Date::now().as_millis() as i64,
        })
    }

    pub fn now(&self) -> String {
        crate::time::rfc3339(self.now_ms)
    }

    pub fn var(&self, name: &str, default: &str) -> String {
        self.env
            .var(name)
            .map(|v| v.to_string())
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_owned())
    }

    pub fn secret(&self, name: &str) -> Option<String> {
        self.env
            .secret(name)
            .map(|v| v.to_string())
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    pub fn langs(&self) -> String {
        self.var("LANGS", "pt-BR")
    }

    pub fn bucket(&self) -> Result<Bucket> {
        self.env.bucket("RELEASES")
    }

    /// Salted hashes of the caller's IP. Without `IP_SALT` set the server
    /// refuses writes (it would otherwise hash with a guessable salt).
    pub fn client(&self, req: &Request) -> std::result::Result<IpHashes, Fail> {
        let salt = self
            .secret("IP_SALT")
            .ok_or_else(|| api_err(503, "server_not_configured"))?;
        let ip = req
            .headers()
            .get("CF-Connecting-IP")?
            .unwrap_or_else(|| "0.0.0.0".into());
        Ok(ip_hashes(&salt, &ip))
    }
}

/// A bound value for D1. D1 takes JS numbers, strings and null.
fn js(v: sea_query::Value) -> JsValue {
    use sea_query::Value as V;
    fn num<T: Into<f64>>(x: Option<T>) -> JsValue {
        x.map(|x| JsValue::from_f64(x.into())).unwrap_or(JsValue::NULL)
    }
    match v {
        V::Bool(x) => x.map(|b| JsValue::from_f64(b as u8 as f64)).unwrap_or(JsValue::NULL),
        V::TinyInt(x) => num(x),
        V::SmallInt(x) => num(x),
        V::Int(x) => num(x),
        V::BigInt(x) => x.map(|x| JsValue::from_f64(x as f64)).unwrap_or(JsValue::NULL),
        V::TinyUnsigned(x) => num(x),
        V::SmallUnsigned(x) => num(x),
        V::Unsigned(x) => num(x),
        V::BigUnsigned(x) => x.map(|x| JsValue::from_f64(x as f64)).unwrap_or(JsValue::NULL),
        V::Float(x) => num(x),
        V::Double(x) => num(x),
        V::String(x) => x.map(|s| JsValue::from_str(&s)).unwrap_or(JsValue::NULL),
        V::Char(x) => x.map(|c| JsValue::from_str(&c.to_string())).unwrap_or(JsValue::NULL),
        // Not produced by `crate::queries`.
        _ => JsValue::NULL,
    }
}

impl Ctx {
    /// Prepares a statement built by [`crate::queries`], binding its values.
    pub fn q(&self, (sql, values): crate::queries::Q) -> Result<D1PreparedStatement> {
        let args: Vec<JsValue> = values.0.into_iter().map(js).collect();
        self.db.prepare(sql).bind(&args)
    }

    /// Prepares several statements (for `db.batch`).
    pub fn qs(&self, qs: Vec<crate::queries::Q>) -> Result<Vec<D1PreparedStatement>> {
        qs.into_iter().map(|q| self.q(q)).collect()
    }
}

/// Sets the security headers of [`crate::headers`] on a response.
pub(crate) fn apply_headers(r: &mut Response, kind: crate::headers::Kind) -> Result<()> {
    let h = r.headers_mut();
    for (k, v) in crate::headers::security_headers(kind) {
        h.set(k, v)?;
    }
    if kind == crate::headers::Kind::Json {
        h.set("content-type", "application/json; charset=utf-8")?;
    }
    Ok(())
}

/// JSON response with `status`, never cached unless the caller says so.
pub(crate) fn json<T: Serialize>(status: u16, body: &T) -> HResult {
    let mut r = Response::from_json(body)?.with_status(status);
    r.headers_mut().set("cache-control", "no-store")?;
    apply_headers(&mut r, crate::headers::Kind::Json)?;
    Ok(r)
}

fn error_response(e: Fail) -> Result<Response> {
    match e {
        Fail::Response(r) => Ok(r),
        Fail::Api(e) => {
            let mut r = Response::from_json(&serde_json::json!({ "error": e.code }))?.with_status(e.status);
            r.headers_mut().set("cache-control", "no-store")?;
            apply_headers(&mut r, crate::headers::Kind::Json)?;
            Ok(r)
        }
        Fail::Internal(e) => {
            console_error!("internal error: {e}");
            let mut r = Response::from_json(&serde_json::json!({ "error": "internal" }))?.with_status(500);
            r.headers_mut().set("cache-control", "no-store")?;
            apply_headers(&mut r, crate::headers::Kind::Json)?;
            Ok(r)
        }
    }
}

/// Reads a request body of at most `limit` bytes (checks `Content-Length`
/// first so oversized bodies are refused before reading them).
pub(crate) async fn body(req: &mut Request, limit: usize) -> std::result::Result<Vec<u8>, Fail> {
    if let Some(len) = req.headers().get("content-length")?
        && len.trim().parse::<usize>().map(|l| l > limit).unwrap_or(true)
    {
        return Err(api_err(413, "body_too_large"));
    }
    let bytes = req.bytes().await?;
    if bytes.len() > limit {
        return Err(api_err(413, "body_too_large"));
    }
    Ok(bytes)
}

#[event(fetch)]
async fn fetch(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let ctx = match Ctx::new(env) {
        Ok(c) => c,
        Err(e) => return error_response(Fail::Internal(e)),
    };
    match route(&mut req, &ctx).await {
        Ok(r) => Ok(r),
        Err(e) => error_response(e),
    }
}

async fn route(req: &mut Request, ctx: &Ctx) -> HResult {
    let path = req.path();
    let segs: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    let m = req.method();
    match (m, segs.as_slice()) {
        (Method::Get | Method::Head, []) => public::site(ctx),
        (Method::Get | Method::Head, ["app.js"]) => public::asset(public::Asset::Js),
        (Method::Get | Method::Head, ["app.css"]) => public::asset(public::Asset::Css),
        (Method::Get | Method::Head, ["fonts", name]) => public::font(name),
        (Method::Get, ["favicon.ico"]) => Ok(Response::empty()?.with_status(204)),
        (Method::Post, ["v1", "contributions"]) => public::contributions(req, ctx).await,
        (Method::Get, ["v1", "texts"]) => public::texts(req, ctx).await,
        (Method::Post, ["v1", "votes"]) => public::vote(req, ctx).await,
        (Method::Post, ["v1", "corrections"]) => public::correction(req, ctx).await,
        (Method::Get, ["v1", "stats"]) => public::stats(req, ctx).await,
        (Method::Get, ["v1", "download"]) => public::download(req, ctx).await,
        (Method::Get, ["v1", "public-key"]) => public::public_key(ctx),
        (Method::Get, ["v1", "releases", lang, "latest"]) => public::latest(ctx, lang).await,
        (Method::Get, ["v1", "releases", lang, version, name]) => public::release_file(ctx, lang, version, name).await,
        (_, ["v1", "admin", rest @ ..]) => admin::route(req, ctx, rest).await,
        (_, ["v1", ..]) => Err(api_err(404, "not_found")),
        _ => Err(api_err(404, "not_found")),
    }
}

/// Cron schedules (keep in sync with `wrangler.toml`).
pub(crate) const CRON_REVIEW: &str = "*/5 * * * *";
pub(crate) const CRON_RELEASE: &str = "0 6 * * *";
pub(crate) const CRON_BACKUP: &str = "30 6 * * *";

#[event(scheduled)]
async fn scheduled(event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    let ctx = match Ctx::new(env) {
        Ok(c) => c,
        Err(e) => return console_error!("scheduled: no context: {e}"),
    };
    let cron = event.cron();
    let result = match cron.as_str() {
        CRON_REVIEW => jobs::review(&ctx)
            .await
            .map(|r| serde_json::to_string(&r).unwrap_or_default()),
        CRON_RELEASE => jobs::build_all_releases(&ctx, false)
            .await
            .map(|r| serde_json::to_string(&r).unwrap_or_default()),
        CRON_BACKUP => jobs::backup(&ctx)
            .await
            .map(|r| serde_json::to_string(&r).unwrap_or_default()),
        other => Err(Error::RustError(format!("unknown cron {other}"))),
    };
    match result {
        Ok(r) => console_log!("cron {cron}: {r}"),
        Err(e) => console_error!("cron {cron} failed: {e}"),
    }
}
