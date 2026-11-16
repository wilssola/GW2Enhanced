//! The latest GitHub release of the mod, reduced to what the site's download list needs (`GET /v1/github/latest`): the
//! version and every file with its direct download link. Pure: the Worker glue fetches and caches.

use serde_json::{Value, json};

/// The repository the releases come from.
pub const REPO: &str = "wilssola/GW2Enhanced";
/// GitHub's API for the latest published release (drafts and pre-releases are not "latest").
pub const LATEST_URL: &str = "https://api.github.com/repos/wilssola/GW2Enhanced/releases/latest";
/// How long a fetched answer is reused (edge cache and the isolate's memory): GitHub allows 60 unauthenticated calls an
/// hour per address, and a release changes rarely.
pub const CACHE_SECS: u64 = 600;
/// A "no release yet" answer is reused for less, so the first release shows up soon.
pub const EMPTY_CACHE_SECS: u64 = 120;

/// GitHub's release JSON -> `{version, tag, published_at, page, assets: [{name, url, size}]}`; `None` when it is not a
/// usable release. Only `https://github.com/<REPO>/releases/download/` links pass, so the site never links elsewhere.
pub fn reduce(v: &Value) -> Option<Value> {
    let tag = v.get("tag_name")?.as_str()?;
    if v.get("draft").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let assets: Vec<Value> = v
        .get("assets")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|a| {
            let name = a.get("name")?.as_str()?;
            let url = a.get("browser_download_url")?.as_str()?;
            if !url.starts_with(&prefix) || name.is_empty() || name.len() > 200 {
                return None;
            }
            Some(json!({ "name": name, "url": url, "size": a.get("size").and_then(Value::as_u64).unwrap_or(0) }))
        })
        .collect();
    let page = format!("https://github.com/{REPO}/releases/tag/{tag}");
    Some(json!({
        "version": tag.trim_start_matches('v'),
        "tag": tag,
        "published_at": v.get("published_at").and_then(Value::as_str).unwrap_or(""),
        "page": page,
        "assets": assets,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_version_and_direct_links_only() {
        let v = json!({
            "tag_name": "v1.2.0", "draft": false, "published_at": "2026-11-16T10:00:00Z",
            "assets": [
                {"name": "GW2Enhanced-Installer-windows-x64.exe", "size": 10,
                 "browser_download_url": "https://github.com/wilssola/GW2Enhanced/releases/download/v1.2.0/GW2Enhanced-Installer-windows-x64.exe"},
                {"name": "GW2Enhanced-All-1.2.0.zip", "size": 20,
                 "browser_download_url": "https://github.com/wilssola/GW2Enhanced/releases/download/v1.2.0/GW2Enhanced-All-1.2.0.zip"},
                {"name": "evil.exe", "size": 1, "browser_download_url": "https://example.com/evil.exe"}
            ]
        });
        let r = reduce(&v).unwrap();
        assert_eq!(r["version"], "1.2.0");
        assert_eq!(r["page"], "https://github.com/wilssola/GW2Enhanced/releases/tag/v1.2.0");
        let a = r["assets"].as_array().unwrap();
        assert_eq!(a.len(), 2, "links outside the repo's release downloads are dropped");
        assert_eq!(a[1]["name"], "GW2Enhanced-All-1.2.0.zip");
        assert_eq!(a[1]["size"], 20);
    }

    #[test]
    fn drafts_and_garbage_are_not_releases() {
        assert!(reduce(&json!({"tag_name": "v1.0.0", "draft": true})).is_none());
        assert!(reduce(&json!({"message": "Not Found"})).is_none());
        assert_eq!(reduce(&json!({"tag_name": "v1.0.0"})).unwrap()["assets"], json!([]));
    }
}
