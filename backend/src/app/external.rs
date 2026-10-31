//! Outbound calls: Cloudflare Turnstile siteverify and NVIDIA NIM.

use serde_json::{Value, json};
use worker::*;

use super::{Ctx, Fail, api_err};
use crate::review::{self, Task, Verdict};

const SITEVERIFY: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";
pub const DEFAULT_NIM_URL: &str = "https://integrate.api.nvidia.com/v1/chat/completions";
pub const DEFAULT_NIM_MODEL: &str = "nvidia/llama-3.3-nemotron-super-49b-v1";

async fn post_json(url: &str, body: &Value, bearer: Option<&str>) -> Result<(u16, Value)> {
    let headers = Headers::new();
    headers.set("content-type", "application/json")?;
    headers.set("accept", "application/json")?;
    if let Some(t) = bearer {
        headers.set("authorization", &format!("Bearer {t}"))?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(body.to_string().into()));
    let req = Request::new_with_init(url, &init)?;
    let mut resp = Fetch::Request(req).send().await?;
    let status = resp.status_code();
    let text = resp.text().await?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// Verifies a Turnstile token server-side; 403 when invalid, 503 when the
/// server has no `TURNSTILE_SECRET`.
pub async fn verify_turnstile(ctx: &Ctx, req: &Request, token: &str) -> std::result::Result<(), Fail> {
    let secret = ctx
        .secret("TURNSTILE_SECRET")
        .ok_or_else(|| api_err(503, "turnstile_not_configured"))?;
    let mut body = json!({ "secret": secret, "response": token });
    if let Some(ip) = req.headers().get("CF-Connecting-IP")? {
        body["remoteip"] = json!(ip);
    }
    let (_, v) = post_json(SITEVERIFY, &body, None).await?;
    if v.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(api_err(403, "turnstile_failed"));
    }
    // Optional: only accept tokens issued for our own site.
    let host = ctx.var("TURNSTILE_HOSTNAME", "");
    if !host.is_empty() && v.get("hostname").and_then(Value::as_str) != Some(host.as_str()) {
        return Err(api_err(403, "turnstile_failed"));
    }
    Ok(())
}

/// Reviews up to [`review::BATCH`] tasks in one chat completion. `Ok(None)`
/// when no API key is configured (items stay queued).
pub async fn ai_review(ctx: &Ctx, tasks: &[Task]) -> Result<Option<Vec<Option<Verdict>>>> {
    let Some(key) = ctx.secret("NVIDIA_API_KEY") else {
        return Ok(None);
    };
    let url = ctx.var("NIM_URL", DEFAULT_NIM_URL);
    let model = ctx.var("NIM_MODEL", DEFAULT_NIM_MODEL);
    let (status, v) = post_json(&url, &review::request_body(&model, tasks), Some(&key)).await?;
    if status != 200 {
        console_warn!(
            "NIM returned {status}: {}",
            v.to_string().chars().take(300).collect::<String>()
        );
        return Ok(Some(vec![None; tasks.len()]));
    }
    let text = review::completion_text(&v).unwrap_or("");
    Ok(Some(review::parse_verdicts(text, tasks.len())))
}
