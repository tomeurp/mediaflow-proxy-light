//! DASH passthrough / CORS-only MPD endpoint.
//!
//! This endpoint does NOT convert DASH to HLS and does NOT decrypt segments.
//! It only fetches the upstream MPD, optionally normalizes BaseURL elements to
//! absolute CDN URLs, optionally strips DRM license metadata that can interfere
//! with ClearKey playback, and returns `application/dash+xml`.
//!
//! Intended route:
//!   /proxy/mpd/dash.mpd?d=<upstream_mpd>&api_password=...&stripdrmmetadata=1
//!
//! Query toggles:
//!   passthrough=1         Return upstream MPD bytes unchanged, only adding CORS.
//!   rewrite_baseurl=0     Do not normalize BaseURL elements. Default: enabled.
//!   stripdrmmetadata=1    Remove PSSH/license-server metadata while preserving
//!                         MP4Protection/default_KID info needed for CENC/ClearKey.
//!   keepdefaultkid=1      Default/implicit. Kept for clarity; default_KID is never stripped.

use std::collections::HashMap;
use std::io::Cursor;
use std::str::FromStr;
use std::sync::atomic::Ordering;

use actix_web::{web, HttpResponse};
use quick_xml::events::{BytesText, Event};
use quick_xml::{Reader, Writer};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::auth::encryption::ProxyData;
use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::metrics::AppMetrics;
use crate::mpd::segment::resolve_url;
use crate::proxy::stream::StreamManager;

fn bool_query(query: &HashMap<String, String>, key: &str, default: bool) -> bool {
    query
        .get(key)
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(default)
}

fn build_request_headers(proxy_data: &ProxyData) -> HeaderMap {
    let mut headers = HeaderMap::new();

    if let Some(map) = proxy_data
        .request_headers
        .as_ref()
        .and_then(|v| v.as_object())
    {
        for (k, v) in map {
            if let Some(val_str) = v.as_str() {
                if let (Ok(name), Ok(value)) =
                    (HeaderName::from_str(k), HeaderValue::from_str(val_str))
                {
                    headers.insert(name, value);
                }
            }
        }
    }

    headers
}

/// Normalize every DASH BaseURL text node to an absolute URL.
///
/// The algorithm tracks inherited DASH BaseURL scope through MPD/Period/
/// AdaptationSet/Representation. When it sees a BaseURL element, it resolves it
/// against the current inherited base and writes the absolute result back into
/// the XML. It also updates the current scope so descendant BaseURL and segment
/// templates inherit correctly.
fn xml_write<T>(result: std::io::Result<T>) -> AppResult<T> {
    result.map_err(|e| serde_json::Error::io(e).into())
}

fn normalize_baseurls_to_absolute(mpd_xml: &[u8], mpd_url: &str) -> AppResult<Vec<u8>> {
    let mut reader = Reader::from_reader(Cursor::new(mpd_xml));
    reader.config_mut().trim_text(false);

    let mut writer = Writer::new(Vec::with_capacity(mpd_xml.len() + 1024));
    let mut buf = Vec::new();

    // BaseURL inheritance stack. Top entry is the current inherited base for the
    // current DASH scope. It starts at the MPD URL so relative top-level BaseURL
    // values resolve against the manifest location.
    let mut base_stack: Vec<String> = vec![mpd_url.to_string()];
    let mut scope_stack: Vec<bool> = Vec::new();
    let mut in_base_url = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let is_scope = matches!(
                    name.as_str(),
                    "MPD" | "Period" | "AdaptationSet" | "Representation"
                );

                if is_scope {
                    let inherited = base_stack
                        .last()
                        .cloned()
                        .unwrap_or_else(|| mpd_url.to_string());
                    base_stack.push(inherited);
                    scope_stack.push(true);
                } else {
                    scope_stack.push(false);
                }

                if name == "BaseURL" {
                    in_base_url = true;
                }

                xml_write(writer.write_event(Event::Start(e.into_owned())))?;
            }
            Ok(Event::Empty(e)) => {
                xml_write(writer.write_event(Event::Empty(e.into_owned())))?;
            }
            Ok(Event::Text(e)) => {
                if in_base_url {
                    let raw = e
                        .unescape()
                        .map_err(|err| AppError::Mpd(format!("Failed to decode BaseURL text: {err}")))?
                        .to_string();
                    let trimmed = raw.trim();
                    let inherited = base_stack
                        .last()
                        .cloned()
                        .unwrap_or_else(|| mpd_url.to_string());
                    let resolved = if trimmed.is_empty() {
                        inherited
                    } else {
                        resolve_url(&inherited, trimmed)
                    };

                    if let Some(top) = base_stack.last_mut() {
                        *top = resolved.clone();
                    }

                    xml_write(writer.write_event(Event::Text(BytesText::new(&resolved))))?;
                } else {
                    xml_write(writer.write_event(Event::Text(e.into_owned())))?;
                }
            }
            Ok(Event::CData(e)) => {
                xml_write(writer.write_event(Event::CData(e.into_owned())))?;
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "BaseURL" {
                    in_base_url = false;
                }

                xml_write(writer.write_event(Event::End(e.into_owned())))?;

                if let Some(is_scope) = scope_stack.pop() {
                    if is_scope && base_stack.len() > 1 {
                        base_stack.pop();
                    }
                }
            }
            Ok(Event::Decl(e)) => xml_write(writer.write_event(Event::Decl(e.into_owned())))?,
            Ok(Event::PI(e)) => xml_write(writer.write_event(Event::PI(e.into_owned())))?,
            Ok(Event::Comment(e)) => xml_write(writer.write_event(Event::Comment(e.into_owned())))?,
            Ok(Event::DocType(e)) => xml_write(writer.write_event(Event::DocType(e.into_owned())))?,
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(AppError::Mpd(format!(
                    "Failed to rewrite DASH BaseURL elements: {e}"
                )))
            }
        }
        buf.clear();
    }

    Ok(writer.into_inner())
}

/// Remove license-server metadata that can make Shaka prefer Widevine/PlayReady
/// license acquisition over explicitly configured ClearKey.
///
/// We intentionally keep the generic mp4protection ContentProtection entries,
/// including cenc:default_KID, because those describe the CENC encryption and are
/// useful/needed for ClearKey.
fn strip_drm_metadata(xml: &[u8]) -> Vec<u8> {
    let mut s = String::from_utf8_lossy(xml).to_string();

    // Remove PSSH boxes but preserve surrounding ContentProtection when possible.
    s = regex::Regex::new(r#"(?is)<cenc:pssh\b[^>]*>.*?</cenc:pssh>"#)
        .unwrap()
        .replace_all(&s, "")
        .into_owned();

    // Remove license URL declarations.
    s = regex::Regex::new(r#"(?is)<clearkey:Laurl\b[^>]*>.*?</clearkey:Laurl>"#)
        .unwrap()
        .replace_all(&s, "")
        .into_owned();
    s = regex::Regex::new(r#"(?is)<ms:laurl\b[^>]*/>"#)
        .unwrap()
        .replace_all(&s, "")
        .into_owned();

    // Remove Widevine/PlayReady ContentProtection blocks entirely. Keep
    // urn:mpeg:dash:mp4protection:2011, where cenc:default_KID normally lives.
    s = regex::Regex::new(
        r#"(?is)<ContentProtection\b(?=[^>]*(?:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed|9a04f079-9840-4286-ab92-e65be0885f95|widevine|playready))[^>]*(?:/>|>.*?</ContentProtection>)"#,
    )
    .unwrap()
    .replace_all(&s, "")
    .into_owned();

    s.into_bytes()
}

/// GET /proxy/mpd/dash.mpd
pub async fn mpd_dash_passthrough_handler(
    stream_manager: web::Data<StreamManager>,
    proxy_data: web::ReqData<ProxyData>,
    _config: web::Data<std::sync::Arc<Config>>,
    metrics: web::Data<std::sync::Arc<AppMetrics>>,
) -> AppResult<HttpResponse> {
    metrics.inc_request();
    metrics.mpd_requests.fetch_add(1, Ordering::Relaxed);

    let destination = proxy_data.destination.clone();
    let query: HashMap<String, String> = proxy_data
        .query_params
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    let passthrough = bool_query(&query, "passthrough", false);
    let rewrite_baseurl = bool_query(&query, "rewrite_baseurl", true);
    let strip_metadata = bool_query(&query, "stripdrmmetadata", false)
        || bool_query(&query, "strip_drm_metadata", false);
    // default_KID is intentionally always preserved.  keepdefaultkid is accepted
    // as a documentation/no-op toggle so callers can make that behavior explicit.
    let _keep_default_kid = bool_query(&query, "keepdefaultkid", true)
        || bool_query(&query, "keep_default_kid", true);

    let request_headers = build_request_headers(&proxy_data);
    let mut body = stream_manager
        .fetch_bytes(destination.clone(), request_headers)
        .await?;

    if !passthrough {
        if rewrite_baseurl {
            body = bytes::Bytes::from(normalize_baseurls_to_absolute(&body, &destination)?);
        }
        if strip_metadata {
            body = bytes::Bytes::from(strip_drm_metadata(&body));
        }
    }

    metrics.add_bytes_out(body.len() as u64);

    Ok(HttpResponse::Ok()
        .content_type("application/dash+xml")
        .insert_header(("cache-control", "no-cache, no-store"))
        .insert_header(("access-control-allow-origin", "*"))
        .insert_header(("access-control-allow-methods", "GET, HEAD, OPTIONS"))
        .insert_header(("access-control-allow-headers", "*"))
        .body(body))
}
