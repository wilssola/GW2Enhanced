//! Security headers for every response (pure, tested on the host).
//!
//! SQL injection is handled by [`crate::queries`] (values always bound). XSS is
//! an output problem: the site only renders API data through `textContent` /
//! DOM APIs, the Worker never builds HTML from data, JSON goes out as
//! `application/json`, and this strict CSP blocks inline and foreign scripts
//! even if a bug slipped through.

/// The site's Content Security Policy. No `'unsafe-inline'`: the JS and CSS are
/// served as `/app.js` and `/app.css`, fonts and images are self-hosted under
/// `/fonts/` and `/img/`.
/// Turnstile needs `challenges.cloudflare.com` in `script-src` and `frame-src`
/// (Cloudflare's Turnstile CSP reference) and `connect-src 'self'`.
pub const CSP: &str = "default-src 'self'; \
script-src 'self' https://challenges.cloudflare.com; \
frame-src https://challenges.cloudflare.com; \
connect-src 'self'; \
img-src 'self' data:; \
font-src 'self'; \
style-src 'self'; \
object-src 'none'; \
base-uri 'none'; \
form-action 'self'; \
frame-ancestors 'none'; \
upgrade-insecure-requests";

/// API responses never render as documents.
pub const CSP_API: &str = "default-src 'none'; frame-ancestors 'none'; base-uri 'none'";

pub const PERMISSIONS_POLICY: &str = "accelerometer=(), autoplay=(), camera=(), display-capture=(), encrypted-media=(), \
fullscreen=(), geolocation=(), gyroscope=(), hid=(), magnetometer=(), microphone=(), midi=(), payment=(), \
picture-in-picture=(), publickey-credentials-get=(), screen-wake-lock=(), serial=(), usb=(), xr-spatial-tracking=(), \
interest-cohort=()";

pub const HSTS: &str = "max-age=63072000; includeSubDomains";

/// What a response carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The site page.
    Html,
    /// API JSON.
    Json,
    /// Static file (`/app.js`, `/app.css`, fonts, images, release files).
    Asset,
}

/// Headers to set on a response of `kind` (content type is set separately).
pub fn security_headers(kind: Kind) -> Vec<(&'static str, &'static str)> {
    let mut h = vec![
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "strict-origin-when-cross-origin"),
        ("strict-transport-security", HSTS),
        ("cross-origin-opener-policy", "same-origin"),
        ("permissions-policy", PERMISSIONS_POLICY),
        ("x-frame-options", "DENY"),
    ];
    match kind {
        Kind::Html => {
            h.push(("content-security-policy", CSP));
            h.push(("cross-origin-resource-policy", "same-origin"));
        }
        Kind::Json => {
            h.push(("content-security-policy", CSP_API));
            h.push(("cross-origin-resource-policy", "same-origin"));
        }
        // Release files are fetched by the mod, not by other sites' pages.
        Kind::Asset => h.push(("cross-origin-resource-policy", "same-origin")),
    }
    h
}

/// Escapes text for an HTML text node or a quoted attribute value. The Worker
/// renders no HTML from data today; anything that ever does must use this.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(kind: Kind, name: &str) -> Option<&'static str> {
        security_headers(kind)
            .into_iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v)
    }

    #[test]
    fn html_has_strict_csp_and_hardening() {
        let csp = get(Kind::Html, "content-security-policy").unwrap();
        assert!(!csp.contains("unsafe-inline") && !csp.contains("unsafe-eval"));
        for d in [
            "default-src 'self'",
            "script-src 'self' https://challenges.cloudflare.com",
            "frame-src https://challenges.cloudflare.com",
            "connect-src 'self'",
            "img-src 'self' data:",
            "style-src 'self'",
            "font-src 'self'",
            "object-src 'none'",
            "base-uri 'none'",
            "form-action 'self'",
            "frame-ancestors 'none'",
            "upgrade-insecure-requests",
        ] {
            assert!(csp.split("; ").any(|x| x == d), "missing {d}");
        }
        for k in [
            "x-content-type-options",
            "referrer-policy",
            "permissions-policy",
            "cross-origin-opener-policy",
            "strict-transport-security",
        ] {
            assert!(get(Kind::Html, k).is_some(), "missing {k}");
        }
        assert!(get(Kind::Html, "permissions-policy").unwrap().contains("camera=()"));
        assert!(get(Kind::Html, "permissions-policy").unwrap().contains("microphone=()"));
        assert!(
            get(Kind::Html, "permissions-policy")
                .unwrap()
                .contains("geolocation=()")
        );
    }

    #[test]
    fn json_and_assets_are_hardened_too() {
        for kind in [Kind::Json, Kind::Asset] {
            assert_eq!(get(kind, "x-content-type-options"), Some("nosniff"));
            assert!(get(kind, "strict-transport-security").is_some());
        }
        assert_eq!(get(Kind::Json, "content-security-policy"), Some(CSP_API));
    }

    /// The CSP forbids inline code; the site must not need it, and the JS must
    /// never turn API data into markup.
    #[test]
    fn site_has_no_inline_code_or_html_sinks() {
        let html = include_str!("../site/index.html");
        let js = include_str!("../site/app.js");
        let css = include_str!("../site/app.css");
        for (i, part) in html.split("<script").enumerate().skip(1) {
            let tag = &part[..part.find('>').unwrap()];
            assert!(tag.contains("src="), "inline <script> #{i}");
            assert!(
                part[part.find('>').unwrap() + 1..]
                    .trim_start()
                    .starts_with("</script>"),
                "script #{i} has a body"
            );
        }
        assert!(!html.contains("<style"), "inline <style>");
        assert!(!html.contains(" style="), "inline style attribute");
        let lower = html.to_lowercase();
        for ev in ["onclick=", "onload=", "onerror=", "oninput=", "javascript:"] {
            assert!(!lower.contains(ev), "inline handler {ev}");
        }
        let code: String = js
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
            "new Function",
        ] {
            assert!(!code.contains(sink), "HTML/JS sink {sink} in app.js");
        }
        // Only same-origin assets (fonts and CSS from 'self').
        assert!(!css.contains("@import") && !css.contains("url(\"http") && !css.contains("url(http"));
    }

    /// Every translatable part of the page (`data-i18n="key"`, `data-i18n-attr="attr:key …"`) has its English
    /// in app.js; the Portuguese is the markup itself.
    #[test]
    fn site_strings_have_english() {
        let html = include_str!("../site/index.html");
        let js = include_str!("../site/app.js");
        let attr = |name: &str| -> Vec<String> {
            let pat = format!(" {name}=\"");
            html.split(pat.as_str())
                .skip(1)
                .map(|rest| rest[..rest.find('"').unwrap()].to_string())
                .collect()
        };
        let mut keys = attr("data-i18n");
        for v in attr("data-i18n-attr") {
            for pair in v.split_whitespace() {
                let (_, key) = pair.split_once(':').expect("data-i18n-attr is attr:key");
                keys.push(key.to_string());
            }
        }
        assert!(keys.len() > 100, "data-i18n markers missing from index.html");
        for k in &keys {
            assert!(js.contains(&format!("\"{k}\":")), "no English for {k} in app.js");
        }
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            escape_html(r#"<img src=x onerror="a('b')">&"#),
            "&lt;img src=x onerror=&quot;a(&#39;b&#39;)&quot;&gt;&amp;"
        );
    }
}
