/// Actix-web route handlers for HLS endpoints.
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use actix_web::{web, HttpRequest, HttpResponse};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::str::FromStr;

use crate::{
    auth::encryption::ProxyData,
    config::Config,
    error::AppResult,
    hls::manifest::{
        error_playlist, graceful_end_playlist, ManifestOptions, ManifestProcessor, ProxyParams,
    },
    hls::prebuffer::HlsPrebuffer,
    metrics::AppMetrics,
    proxy::stream::StreamManager,
    utils::url::public_proxy_base_url,
};

/// Extract passthrough params from `proxy_data`:
/// - `api_password` from query params inside `proxy_data.query_params`
/// - `h_*` request headers
fn extract_proxy_params(proxy_data: &ProxyData, config: &Config) -> ProxyParams {
    let api_password = config.auth.api_password.clone();

    let pass_headers = proxy_data
        .request_headers
        .as_ref()
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    ProxyParams::new(&api_password, pass_headers)
}

// ---------------------------------------------------------------------------
// Route: GET /proxy/hls/manifest
// ---------------------------------------------------------------------------

/// Fetch an upstream M3U8 playlist, rewrite URLs, and return the modified content.
pub async fn hls_manifest_handler(
    req: HttpRequest,
    stream_manager: web::Data<StreamManager>,
    proxy_data: web::ReqData<ProxyData>,
    config: web::Data<Arc<Config>>,
    metrics: web::Data<Arc<AppMetrics>>,
    hls_prebuffer: web::Data<HlsPrebuffer>,
) -> AppResult<HttpResponse> {
    metrics.inc_request();
    metrics.hls_requests.fetch_add(1, Ordering::Relaxed);
    let destination = proxy_data.destination.clone();
    let proxy_base = public_proxy_base_url(&req, &config.server.path);
    let params = extract_proxy_params(&proxy_data, &config).with_playlist_url(&destination);

    // Extract manifest-processing options from query params
    let query_params: HashMap<String, String> =
        web::Query::<HashMap<String, String>>::from_query(req.query_string())
            .map(|q| q.into_inner())
            .unwrap_or_default();

    let opts = ManifestOptions {
        key_only_proxy: query_params
            .get("key_only_proxy")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
        no_proxy: query_params
            .get("no_proxy")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
        force_playlist_proxy: query_params
            .get("force_playlist_proxy")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false),
        start_offset: query_params
            .get("start_offset")
            .and_then(|v| v.parse::<f64>().ok()),
        force_start_offset: query_params.contains_key("start_offset"),
        skip_ranges: Vec::new(), // TODO: parse from query params in future
    };

    // Build request headers for upstream fetch
    let mut request_headers = HeaderMap::new();
    for (k, v) in &params.pass_headers {
        if let (Ok(name), Ok(value)) = (HeaderName::from_str(k), HeaderValue::from_str(v)) {
            request_headers.insert(name, value);
        }
    }

    // Fetch the upstream M3U8
    let upstream_bytes = stream_manager
        .fetch_bytes(destination.clone(), request_headers)
        .await
        .map_err(|e| {
            tracing::warn!("Failed to fetch HLS manifest from {}: {}", destination, e);
            e
        });

    let content = match upstream_bytes {
        Ok(bytes) => bytes,
        Err(_) => {
            let body = graceful_end_playlist("Stream unavailable");
            metrics.add_bytes_out(body.len() as u64);
            return Ok(HttpResponse::Ok()
                .content_type("application/vnd.apple.mpegurl")
                .body(body));
        }
    };

    // Process the M3U8
    // force_playlist_proxy routes all media entries as playlists, so segment
    // prebuffering would register unsafe/non-segment URLs.
    if req.method() == actix_web::http::Method::GET
        && !opts.no_proxy
        && !opts.key_only_proxy
        && !opts.force_playlist_proxy
    {
        let segment_urls = ManifestProcessor::media_segment_urls(&content, &destination)
            .into_iter()
            .take(config.hls.prebuffer_segments)
            .collect::<Vec<_>>();
        if !segment_urls.is_empty() {
            let prebuffer = hls_prebuffer.clone();
            let playlist_url = destination.clone();
            let headers = params.pass_headers.clone();
            tokio::spawn(async move {
                prebuffer
                    .register_playlist(&playlist_url, segment_urls, headers)
                    .await;
            });
        }
    }

    let processor = ManifestProcessor::new(&proxy_base, params, opts);
    let processed = processor.process(&content, &destination);

    // Validate that we got a real M3U8
    if !processed.contains("#EXTM3U") {
        let body = error_playlist("Invalid upstream response");
        metrics.add_bytes_out(body.len() as u64);
        return Ok(HttpResponse::Ok()
            .content_type("application/vnd.apple.mpegurl")
            .body(body));
    }

    metrics.add_bytes_out(processed.len() as u64);
    Ok(HttpResponse::Ok()
        .content_type("application/vnd.apple.mpegurl")
        .insert_header(("cache-control", "no-cache, no-store"))
        .body(processed))
}

// ---------------------------------------------------------------------------
// Route: GET /proxy/hls/playlist  (alias / sub-playlist endpoint)
// ---------------------------------------------------------------------------

/// Same logic as `hls_manifest_handler` — used for sub-playlist fetches.
pub async fn hls_playlist_handler(
    req: HttpRequest,
    stream_manager: web::Data<StreamManager>,
    proxy_data: web::ReqData<ProxyData>,
    config: web::Data<Arc<Config>>,
    metrics: web::Data<Arc<AppMetrics>>,
    hls_prebuffer: web::Data<HlsPrebuffer>,
) -> AppResult<HttpResponse> {
    hls_manifest_handler(
        req,
        stream_manager,
        proxy_data,
        config,
        metrics,
        hls_prebuffer,
    )
    .await
}
