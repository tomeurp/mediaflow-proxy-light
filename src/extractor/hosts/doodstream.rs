use async_trait::async_trait;
use regex::Regex;
use rquest::header::{HeaderMap, HeaderName, HeaderValue};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::Duration;

use crate::extractor::base::{
    build_chrome_client, BaseExtractor, ExtraParams, Extractor, ExtractorError, ExtractorResult,
};

const DOOD_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/133.0.0.0 Safari/537.36";

fn pass_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(/pass_md5/[^'"<>\s]+)"#).unwrap())
}
fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"token=([^&\s'"]+)"#).unwrap())
}

pub struct DoodStreamExtractor {
    pub base: BaseExtractor,
    /// rquest client with Chrome TLS/HTTP2 fingerprint — bypasses Cloudflare bot detection.
    chrome_client: rquest::Client,
    /// Optional Byparr service URL for Cloudflare/Turnstile bypass.
    byparr_url: Option<String>,
    byparr_timeout: u64,
}

impl DoodStreamExtractor {
    pub fn new(
        request_headers: HashMap<String, String>,
        proxy_url: Option<String>,
        byparr_url: Option<String>,
        byparr_timeout: u64,
    ) -> Self {
        let chrome_client = build_chrome_client(proxy_url.as_deref());
        Self {
            base: BaseExtractor::new(request_headers, proxy_url),
            chrome_client,
            byparr_url,
            byparr_timeout,
        }
    }

    async fn chrome_get(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<(String, String), ExtractorError> {
        let resp = self
            .chrome_client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| ExtractorError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status >= 400 {
            return Err(ExtractorError::Http {
                status,
                message: format!("HTTP {status} from {url}"),
            });
        }

        let final_url = resp.url().to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| ExtractorError::Network(e.to_string()))?;
        Ok((text, final_url))
    }

    async fn parse_embed_html(
        &self,
        html: &str,
        base_url: &str,
        ua: &str,
    ) -> Result<ExtractorResult, ExtractorError> {
        let pass_path = pass_re()
            .find(html)
            .ok_or_else(|| ExtractorError::extract("Doodstream: pass_md5 path not found"))?
            .as_str();
        let pass_url = format!("{base_url}{pass_path}");

        let mut fetch_hm = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(ua) {
            fetch_hm.insert(HeaderName::from_static("user-agent"), v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("{base_url}/")) {
            fetch_hm.insert(HeaderName::from_static("referer"), v);
        }

        let (base_stream, _) = self.chrome_get(&pass_url, fetch_hm).await?;
        let base_stream = base_stream.trim().to_string();

        if base_stream.is_empty() || base_stream.contains("RELOAD") {
            return Err(ExtractorError::extract(
                "Doodstream: pass_md5 endpoint returned no stream URL \
                 (captcha session may have expired).",
            ));
        }

        let token = token_re()
            .captures(html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
            .ok_or_else(|| ExtractorError::extract("Doodstream: token not found in embed HTML"))?;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let final_stream_url = format!("{base_stream}123456789?token={token}&expiry={now}");

        let mut result_headers = HashMap::new();
        result_headers.insert("user-agent".to_string(), ua.to_string());
        result_headers.insert("referer".to_string(), format!("{base_url}/"));

        Ok(ExtractorResult {
            destination_url: final_stream_url,
            request_headers: result_headers,
            mediaflow_endpoint: "proxy_stream_endpoint",
        })
    }

    async fn extract_via_byparr(
        &self,
        embed_url: &str,
        video_id: &str,
    ) -> Result<ExtractorResult, ExtractorError> {
        let byparr_endpoint = format!(
            "{}/v1",
            self.byparr_url.as_deref().unwrap().trim_end_matches('/')
        );
        let payload = serde_json::json!({
            "cmd": "request.get",
            "url": embed_url,
            "maxTimeout": self.byparr_timeout * 1000,
        });

        let resp = self
            .base
            .client
            .post(&byparr_endpoint)
            .json(&payload)
            .timeout(Duration::from_secs(self.byparr_timeout + 15))
            .send()
            .await
            .map_err(|e| ExtractorError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status != 200 {
            return Err(ExtractorError::extract(format!("Byparr HTTP {status}")));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ExtractorError::extract(format!("Byparr: JSON parse error: {e}")))?;

        if data.get("status").and_then(|s| s.as_str()) != Some("ok") {
            return Err(ExtractorError::extract(format!(
                "Byparr: {}",
                data.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
            )));
        }

        let solution = &data["solution"];
        let final_url_str = solution
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or(embed_url);
        let final_url = if final_url_str.starts_with("http") {
            final_url_str
        } else {
            embed_url
        };
        let base_url = base_from_url(final_url);
        let html = solution
            .get("response")
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let ua = solution
            .get("userAgent")
            .and_then(|u| u.as_str())
            .unwrap_or(DOOD_UA);

        if html.contains("pass_md5") {
            return self.parse_embed_html(html, &base_url, ua).await;
        }

        // Try cookie reuse with Chrome client
        let raw_cookies = solution.get("cookies").and_then(|c| c.as_array());
        if let Some(cookies) = raw_cookies {
            if !cookies.is_empty() {
                let cf_domain = cookies
                    .iter()
                    .find(|c| c.get("name").and_then(|n| n.as_str()) == Some("cf_clearance"))
                    .and_then(|c| c.get("domain").and_then(|d| d.as_str()))
                    .map(|d| d.trim_start_matches('.').to_string())
                    .unwrap_or_else(|| "playmogo.com".to_string());

                let retry_url = format!("https://{cf_domain}/e/{video_id}");
                let cookie_header: String = cookies
                    .iter()
                    .filter_map(|c| {
                        let name = c.get("name")?.as_str()?;
                        let value = c.get("value")?.as_str()?;
                        Some(format!("{name}={value}"))
                    })
                    .collect::<Vec<_>>()
                    .join("; ");

                let mut hm = HeaderMap::new();
                if let Ok(v) = HeaderValue::from_str(ua) {
                    hm.insert(HeaderName::from_static("user-agent"), v);
                }
                if let Ok(v) = HeaderValue::from_str(&format!("https://{cf_domain}/")) {
                    hm.insert(HeaderName::from_static("referer"), v);
                }
                if !cookie_header.is_empty() {
                    if let Ok(v) = HeaderValue::from_str(&cookie_header) {
                        hm.insert(HeaderName::from_static("cookie"), v);
                    }
                }

                if let Ok((retry_html, retry_final)) = self.chrome_get(&retry_url, hm).await {
                    if retry_html.contains("pass_md5") {
                        let retry_base = base_from_url(&retry_final);
                        return self.parse_embed_html(&retry_html, &retry_base, ua).await;
                    }
                }
            }
        }

        // Fall back to Chrome impersonation
        self.extract_via_chrome(embed_url, video_id).await
    }

    async fn extract_via_chrome(
        &self,
        url: &str,
        video_id: &str,
    ) -> Result<ExtractorResult, ExtractorError> {
        let embed_url = embed_url_from_raw(url, video_id);
        let origin_host = url_host(url);

        let mut hm = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(DOOD_UA) {
            hm.insert(HeaderName::from_static("user-agent"), v);
        }
        if let Ok(v) = HeaderValue::from_str(&format!("https://{origin_host}/")) {
            hm.insert(HeaderName::from_static("referer"), v);
        }

        let (html, final_url) = self.chrome_get(&embed_url, hm).await?;
        let base_url = base_from_url(&final_url);

        if !html.contains("pass_md5") {
            if html.contains("turnstile") || html.contains("captcha_l") {
                return Err(ExtractorError::extract(
                    "Doodstream: site is serving a Turnstile CAPTCHA that requires \
                     browser interaction — cannot be bypassed automatically from this \
                     network location. Try a residential IP or a VPN/proxy.",
                ));
            }
            return Err(ExtractorError::extract(format!(
                "Doodstream: pass_md5 not found in embed HTML (final URL: {final_url})"
            )));
        }

        self.parse_embed_html(&html, &base_url, DOOD_UA).await
    }
}

#[async_trait]
impl Extractor for DoodStreamExtractor {
    fn host_name(&self) -> &'static str {
        "Doodstream"
    }

    async fn extract(
        &self,
        url: &str,
        _extra: &ExtraParams,
    ) -> Result<ExtractorResult, ExtractorError> {
        let video_id = url
            .trim_end_matches('/')
            .split('/')
            .next_back()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ExtractorError::extract("Doodstream: invalid URL — no video ID"))?;

        let embed_url = embed_url_from_raw(url, video_id);

        if self.byparr_url.is_some() {
            return self.extract_via_byparr(&embed_url, video_id).await;
        }

        self.extract_via_chrome(url, video_id).await
    }
}

fn url_host(url: &str) -> &str {
    url.trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or("dood.to")
}

fn embed_url_from_raw(url: &str, video_id: &str) -> String {
    if url.contains("/e/") {
        return url.to_string();
    }
    let host = url_host(url);
    format!("https://{host}/e/{video_id}")
}

fn base_from_url(url: &str) -> String {
    let stripped = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = stripped.split('/').next().unwrap_or("playmogo.com");
    format!("https://{host}")
}
