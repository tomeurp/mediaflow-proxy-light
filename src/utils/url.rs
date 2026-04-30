/// URL joining and manipulation utilities.
use actix_web::HttpRequest;
use url::Url;

/// Join a potentially relative `path` against an absolute `base_url`.
///
/// If `path` is already absolute (starts with a scheme), return it as-is.
/// Falls back to string concatenation if parsing fails.
pub fn resolve_url(base_url: &str, path: &str) -> String {
    // Already absolute
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_string();
    }

    // Use url::Url for proper RFC 3986 resolution
    match Url::parse(base_url) {
        Ok(base) => match base.join(path) {
            Ok(resolved) => resolved.to_string(),
            Err(_) => {
                // Fallback: naive join
                let base_str = base_url.trim_end_matches('/');
                let path_str = path.trim_start_matches('/');
                format!("{}/{}", base_str, path_str)
            }
        },
        Err(_) => path.to_string(),
    }
}

/// Detect the segment file extension from a URL path (e.g. "ts", "m4s", "mp4").
/// Defaults to "ts" for unknown/missing extensions.
pub fn segment_extension(url: &str) -> &'static str {
    let lower = url.to_lowercase();
    // Strip query string for extension detection
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".m4s") {
        "m4s"
    } else if path.ends_with(".mp4") {
        "mp4"
    } else if path.ends_with(".m4a") {
        "m4a"
    } else if path.ends_with(".m4v") {
        "m4v"
    } else if path.ends_with(".aac") {
        "aac"
    } else {
        "ts"
    }
}

/// Extract the scheme and authority (host[:port]) from a URL string.
/// Returns e.g. `("https", "example.com:8080")`.
pub fn scheme_and_authority(url: &str) -> Option<(String, String)> {
    let parsed = Url::parse(url).ok()?;
    let scheme = parsed.scheme().to_string();
    let host = parsed.host_str()?.to_string();
    let authority = match parsed.port() {
        Some(port) => format!("{}:{}", host, port),
        None => host,
    };
    Some((scheme, authority))
}

/// Normalize a public path prefix from config/env.
///
/// Empty, `/`, and whitespace-only values mean no prefix. Non-empty values are
/// returned with one leading slash and no trailing slash.
pub fn normalize_public_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// Build the public-facing base URL used by manifest and API rewriters.
///
/// This keeps reverse-proxy path prefixes explicit via `APP__SERVER__PATH`,
/// while still respecting forwarded host/proto headers when present.
pub fn public_proxy_base_url(req: &HttpRequest, public_path: &str) -> String {
    let conn = req.connection_info();
    let scheme = req
        .headers()
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| conn.scheme());
    let host = req
        .headers()
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| conn.host());
    format!("{scheme}://{host}{}", normalize_public_path(public_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_absolute() {
        let resolved = resolve_url("https://example.com/base/", "https://other.com/path");
        assert_eq!(resolved, "https://other.com/path");
    }

    #[test]
    fn test_resolve_relative() {
        let resolved = resolve_url("https://example.com/base/playlist.m3u8", "segment001.ts");
        assert_eq!(resolved, "https://example.com/base/segment001.ts");
    }

    #[test]
    fn test_resolve_absolute_path() {
        let resolved = resolve_url("https://example.com/base/", "/segments/001.ts");
        assert_eq!(resolved, "https://example.com/segments/001.ts");
    }

    #[test]
    fn test_segment_extension() {
        assert_eq!(segment_extension("https://cdn.example.com/seg001.ts"), "ts");
        assert_eq!(
            segment_extension("https://cdn.example.com/seg001.m4s?t=123"),
            "m4s"
        );
        assert_eq!(segment_extension("https://cdn.example.com/init.mp4"), "mp4");
    }

    #[test]
    fn test_normalize_public_path() {
        assert_eq!(normalize_public_path(""), "");
        assert_eq!(normalize_public_path("/"), "");
        assert_eq!(
            normalize_public_path("mediaflow/prefix"),
            "/mediaflow/prefix"
        );
        assert_eq!(
            normalize_public_path("/mediaflow/prefix/"),
            "/mediaflow/prefix"
        );
    }
}
