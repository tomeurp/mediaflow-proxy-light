//! F16Px extractor — POST-based API with AES-GCM encrypted sources.
//!
//! Reference: https://github.com/Gujal00/ResolveURL (plugins/f16px.py)
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use rand::RngCore;
use regex::Regex;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::extractor::base::{
    BaseExtractor, ExtraParams, Extractor, ExtractorError, ExtractorResult,
};

type HmacSha256 = Hmac<Sha256>;

fn embed_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/e/([A-Za-z0-9]+)").unwrap())
}

pub struct F16PxExtractor(pub BaseExtractor);

impl F16PxExtractor {
    pub fn new(request_headers: HashMap<String, String>, proxy_url: Option<String>) -> Self {
        Self(BaseExtractor::new(request_headers, proxy_url))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn b64url_encode(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn b64url_decode(s: &str) -> Result<Vec<u8>, ExtractorError> {
    let s = s.replace('-', "+").replace('_', "/");
    let pad = (4 - s.len() % 4) % 4;
    let padded = format!("{s}{}", "=".repeat(pad));
    base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .map_err(|e| ExtractorError::extract(format!("F16Px: base64 decode error: {e}")))
}

fn join_key_parts(parts: &[&str]) -> Result<Vec<u8>, ExtractorError> {
    let mut key = Vec::new();
    for p in parts {
        key.extend(b64url_decode(p)?);
    }
    Ok(key)
}

fn pick_best(sources: &[serde_json::Value]) -> Option<String> {
    sources
        .iter()
        .max_by_key(|s| {
            s.get("label")
                .and_then(|l| l.as_str())
                .and_then(|l| l.parse::<i64>().ok())
                .unwrap_or(0)
        })
        .and_then(|s| s.get("url"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
}

fn make_fingerprint() -> serde_json::Value {
    let mut viewer_bytes = [0u8; 16];
    let mut device_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut viewer_bytes);
    rand::rng().fill_bytes(&mut device_bytes);

    let viewer_id = b64url_encode(&viewer_bytes);
    let device_id = b64url_encode(&device_bytes);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Build payload JSON string with deterministic key order (matches Python json.dumps).
    let payload_str = format!(
        r#"{{"viewer_id":"{viewer_id}","device_id":"{device_id}","confidence":0.93,"iat":{now},"exp":{}}}"#,
        now + 600
    );
    let payload_b64 = b64url_encode(payload_str.as_bytes());

    // HMAC-SHA256 with empty key (mirrors Python: hmac.new(b"", ..., sha256))
    let mut mac = HmacSha256::new_from_slice(b"").expect("HMAC accepts any key size");
    mac.update(payload_b64.as_bytes());
    let sig = mac.finalize().into_bytes();

    let token = format!("{}.{}", payload_b64, b64url_encode(&sig));

    serde_json::json!({
        "fingerprint": {
            "token": token,
            "viewer_id": viewer_id,
            "device_id": device_id,
            "confidence": 0.93,
        }
    })
}

fn aes_gcm_decrypt(key: &[u8], iv: &[u8], payload: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::{aead::Aead, Aes128Gcm, Aes256Gcm, KeyInit, Nonce};

    if iv.len() != 12 || payload.len() < 16 {
        return None;
    }
    let nonce = Nonce::from_slice(iv);

    match key.len() {
        16 => {
            let cipher = Aes128Gcm::new_from_slice(key).ok()?;
            cipher.decrypt(nonce, payload).ok()
        }
        32 => {
            let cipher = Aes256Gcm::new_from_slice(key).ok()?;
            cipher.decrypt(nonce, payload).ok()
        }
        _ => None,
    }
}

fn decrypt_playback(pb: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    // Primary: iv + key_parts + payload
    let iv = pb.get("iv").and_then(|v| v.as_str())?;
    let key_parts: Vec<&str> = pb
        .get("key_parts")
        .and_then(|v| v.as_array())?
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let payload_b64 = pb.get("payload").and_then(|v| v.as_str())?;

    let iv_bytes = b64url_decode(iv).ok()?;
    let key_bytes = join_key_parts(&key_parts).ok()?;
    let payload_bytes = b64url_decode(payload_b64).ok()?;

    if let Some(plain) = aes_gcm_decrypt(&key_bytes, &iv_bytes, &payload_bytes) {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&plain) {
            if let Some(sources) = json.get("sources").and_then(|s| s.as_array()) {
                if !sources.is_empty() {
                    return Some(sources.clone());
                }
            }
        }
    }

    // Fallback: payload2 + decrypt_keys
    let iv2 = pb.get("iv2").and_then(|v| v.as_str())?;
    let payload2 = pb.get("payload2").and_then(|v| v.as_str())?;
    let decrypt_keys = pb.get("decrypt_keys").and_then(|v| v.as_object())?;

    let iv2_bytes = b64url_decode(iv2).ok()?;
    let payload2_bytes = b64url_decode(payload2).ok()?;

    for key_b64 in decrypt_keys.values() {
        if let Some(key_str) = key_b64.as_str() {
            if let Ok(key2) = b64url_decode(key_str) {
                if let Some(plain) = aes_gcm_decrypt(&key2, &iv2_bytes, &payload2_bytes) {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&plain) {
                        if let Some(sources) = json.get("sources").and_then(|s| s.as_array()) {
                            if !sources.is_empty() {
                                return Some(sources.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Extractor impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Extractor for F16PxExtractor {
    fn host_name(&self) -> &'static str {
        "F16Px"
    }

    async fn extract(
        &self,
        url: &str,
        _extra: &ExtraParams,
    ) -> Result<ExtractorResult, ExtractorError> {
        let scheme_host: String = url.split('/').take(3).collect::<Vec<_>>().join("/");
        let host = url.split('/').nth(2).unwrap_or("");

        let media_id = embed_id_re()
            .captures(url)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .ok_or_else(|| ExtractorError::extract("F16Px: invalid embed URL"))?;

        let api_url = format!("https://{host}/api/videos/{media_id}/embed/playback");

        let mut extra_headers = HashMap::new();
        extra_headers.insert("referer".to_string(), format!("{scheme_host}/e/{media_id}"));
        extra_headers.insert("origin".to_string(), scheme_host.clone());
        extra_headers.insert("content-type".to_string(), "application/json".to_string());

        let (json_str, _) = self
            .0
            .post_json_text(&api_url, make_fingerprint(), Some(extra_headers.clone()))
            .await?;

        let data: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| ExtractorError::extract(format!("F16Px: JSON parse error: {e}")))?;

        // Case 1: plain sources — pick highest quality.
        if let Some(sources) = data["sources"].as_array() {
            if !sources.is_empty() {
                let best = pick_best(sources)
                    .ok_or_else(|| ExtractorError::extract("F16Px: empty source URL"))?;
                return Ok(ExtractorResult {
                    destination_url: best,
                    request_headers: extra_headers,
                    mediaflow_endpoint: "hls_manifest_proxy",
                });
            }
        }

        // Case 2: encrypted playback.
        let pb = data
            .get("playback")
            .ok_or_else(|| ExtractorError::extract("F16Px: no playback data"))?;

        let sources = decrypt_playback(pb)
            .ok_or_else(|| ExtractorError::extract("F16Px: decryption failed / no sources"))?;

        let best = pick_best(&sources)
            .ok_or_else(|| ExtractorError::extract("F16Px: empty source URL after decryption"))?;

        let mut out_headers = HashMap::new();
        out_headers.insert("referer".to_string(), format!("{scheme_host}/"));
        out_headers.insert("origin".to_string(), scheme_host);
        out_headers.insert("Accept-Language".to_string(), "en-US,en;q=0.5".to_string());
        out_headers.insert("Accept".to_string(), "*/*".to_string());
        out_headers.insert(
            "user-agent".to_string(),
            "Mozilla/5.0 (X11; Linux x86_64; rv:138.0) Gecko/20100101 Firefox/138.0".to_string(),
        );

        Ok(ExtractorResult {
            destination_url: best,
            request_headers: out_headers,
            mediaflow_endpoint: "hls_manifest_proxy",
        })
    }
}
