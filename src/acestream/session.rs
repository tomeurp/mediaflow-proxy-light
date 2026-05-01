//! Acestream session registry.
//!
//! Manages sessions with the local Acestream engine. Each session is started
//! by calling `/ace/manifest.m3u8?format=json&pid=<uuid>` which returns the
//! engine-assigned `playback_url` (containing the `access_token`) plus
//! `stat_url` and `command_url` for lifecycle management.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::Stream;

use dashmap::DashMap;
use serde::Deserialize;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Engine JSON response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct EngineJsonResponse {
    response: Option<EngineResponse>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EngineResponse {
    playback_url: Option<String>,
    stat_url: Option<String>,
    command_url: Option<String>,
    #[serde(default)]
    is_live: u8,
    playback_session_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Session data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AcestreamSession {
    pub infohash: String,
    pub pid: String,
    /// Full playback URL. In premium/JSON mode contains `access_token`.
    /// In free mode this is just `http://127.0.0.1:<port>/ace/manifest.m3u8?id=<id>`.
    pub playback_url: String,
    pub command_url: Option<String>,
    pub stat_url: Option<String>,
    pub playback_session_id: Option<String>,
    pub is_live: bool,
    /// True when the engine's JSON API was unavailable (premium required).
    /// In free mode only HLS manifest works; getstream returns 500.
    pub is_free_mode: bool,
    pub created_at: Instant,
    pub last_access: Instant,
    /// Number of active streaming clients sharing this engine session.
    pub client_count: Arc<AtomicUsize>,
}

impl AcestreamSession {
    pub fn touch(&mut self) {
        self.last_access = Instant::now();
    }

    pub fn is_stale(&self, ttl: Duration) -> bool {
        self.last_access.elapsed() > ttl
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AcestreamSessionManager {
    sessions: Arc<DashMap<String, AcestreamSession>>,
    /// Short-lived HTTP client for engine control calls (stat, stop, session init).
    client: reqwest::Client,
    /// Streaming HTTP client for /ace/getstream — NO overall timeout so a live
    /// MPEG-TS connection is never killed by the request_timeout_factor limit.
    /// Only the connect timeout is set (guards against engine startup delays).
    stream_client: reqwest::Client,
}

impl Default for AcestreamSessionManager {
    fn default() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            stream_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(30))
                // Deliberately no .timeout() — live MPEG-TS streams run indefinitely.
                .no_gzip()
                .no_deflate()
                .no_brotli()
                .build()
                .unwrap_or_default(),
        }
    }
}

impl AcestreamSessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get an existing live session or start a new one with the engine.
    pub async fn get_or_create(
        &self,
        engine_host: &str,
        engine_port: u16,
        infohash: &str,
        content_id: Option<&str>,
        access_token: Option<&str>,
    ) -> Result<AcestreamSession, String> {
        // Return cached session if still live
        if let Some(mut entry) = self.sessions.get_mut(infohash) {
            if !entry.is_stale(Duration::from_secs(300)) {
                entry.touch();
                return Ok(entry.clone());
            }
        }

        // Start a fresh session with the engine.
        // Try the JSON API first (returns access_token for premium / unrestricted engines).
        // If the engine requires premium for the JSON API, fall back to direct HLS mode
        // which works on free Android builds (stream plays with engine-injected ads).
        let pid = Uuid::new_v4().to_string();

        // Append static access_token if configured (required on some Android engine builds)
        let token_suffix = access_token
            .filter(|t| !t.is_empty())
            .map(|t| format!("&token={t}"))
            .unwrap_or_default();

        // Mirror Python's parameter name choice:
        //   content_id provided (user sent ?id=xxx)  → use `id=`
        //   no content_id (user sent ?infohash=xxx)  → use `infohash=`
        let (id_key, id_param) = match content_id {
            Some(cid) => ("id", cid),
            None => ("infohash", infohash),
        };

        let json_url = format!(
            "http://{engine_host}:{engine_port}/ace/manifest.m3u8\
             ?format=json&pid={pid}&{id_key}={id_param}{token_suffix}"
        );

        tracing::debug!("Acestream JSON init: {json_url}");

        let (playback_url, command_url, stat_url, is_free_mode) = match self
            .try_json_session(&json_url, engine_host, engine_port)
            .await
        {
            Ok((url, cmd, stat)) => (url, cmd, stat, false),
            Err(e) => {
                tracing::warn!("Acestream JSON init error: {e}");
                if is_premium_error(&e) {
                    tracing::info!(
                        "Acestream engine requires premium for JSON API — \
                             falling back to direct HLS mode (free tier)"
                    );
                    // Free mode: only manifest.m3u8 works; getstream returns 500.
                    let url = format!(
                            "http://{engine_host}:{engine_port}/ace/manifest.m3u8?{id_key}={id_param}{token_suffix}"
                        );
                    // Synthetic command URL for free mode
                    let cmd = Some(format!(
                        "http://{engine_host}:{engine_port}/ace/cmd?pid={pid}"
                    ));
                    let stat = Some(format!("http://{engine_host}:{engine_port}/ace/stat/{pid}"));
                    (url, cmd, stat, true)
                } else {
                    return Err(e);
                }
            }
        };

        let session = AcestreamSession {
            infohash: infohash.to_string(),
            pid,
            playback_url,
            command_url,
            stat_url,
            playback_session_id: None,
            is_live: true,
            is_free_mode,
            created_at: Instant::now(),
            last_access: Instant::now(),
            client_count: Arc::new(AtomicUsize::new(0)),
        };

        self.sessions.insert(infohash.to_string(), session.clone());
        Ok(session)
    }

    /// Try the JSON session init endpoint, returning (playback_url, command_url, stat_url).
    async fn try_json_session(
        &self,
        url: &str,
        engine_host: &str,
        engine_port: u16,
    ) -> Result<(String, Option<String>, Option<String>), String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| format!("Engine request failed: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("Engine returned HTTP {}", resp.status()));
        }

        let body: EngineJsonResponse = resp
            .json()
            .await
            .map_err(|e| format!("Engine JSON parse error: {e}"))?;

        if let Some(err) = body.error {
            return Err(format!("premium:{err}"));
        }

        let engine = body
            .response
            .ok_or_else(|| "Engine response missing 'response' field".to_string())?;

        let playback_url = engine
            .playback_url
            .ok_or_else(|| "Engine response missing playback_url".to_string())?;

        // The acestream engine always embeds its own hostname in the returned URLs
        // (e.g. "http://localhost:6878/ace/m/...").  When the engine is running on
        // a remote host we must rewrite those references so that subsequent fetches
        // (manifest, stat, command) go to the right machine instead of the
        // proxy's own localhost.
        Ok((
            rewrite_engine_host(playback_url, engine_host, engine_port),
            engine
                .command_url
                .map(|s| rewrite_engine_host(s, engine_host, engine_port)),
            engine
                .stat_url
                .map(|s| rewrite_engine_host(s, engine_host, engine_port)),
        ))
    }

    /// Open a raw MPEG-TS stream from the Acestream engine's getstream endpoint.
    ///
    /// Uses the no-timeout `stream_client` so the connection is not killed by
    /// the proxy's `request_timeout_factor` limit (which only guards short-lived
    /// manifest/segment fetches, not indefinite live streams).
    pub async fn open_ts_stream(
        &self,
        ts_url: &str,
    ) -> Result<impl Stream<Item = Result<Bytes, String>>, String> {
        let response = self
            .stream_client
            .get(ts_url)
            .send()
            .await
            .map_err(|e| format!("Acestream getstream connect failed: {e}"))?;

        if !response.status().is_success() {
            return Err(format!(
                "Acestream getstream returned HTTP {}",
                response.status()
            ));
        }

        use futures::StreamExt;
        Ok(response
            .bytes_stream()
            .map(|r| r.map_err(|e| format!("Acestream stream read error: {e}"))))
    }

    /// Increment the client count for a session (call once per active stream consumer).
    pub fn increment_client(&self, infohash: &str) {
        if let Some(s) = self.sessions.get(infohash) {
            s.client_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Decrement the client count; stop the engine session only when the last client leaves.
    ///
    /// The session is removed from the map **immediately** when the last client disconnects
    /// so that any reconnecting client gets a fresh session with a new `pid`.  Reusing an
    /// old `pid` after the engine closes its getstream connection causes the engine to serve
    /// only the remaining buffer (a few seconds) instead of starting a fresh live stream.
    ///
    /// The engine stop command itself is delayed by 30 seconds: if the reconnect creates a
    /// new session before the timer fires, the stop is skipped (the new session owns the
    /// engine lifecycle).  This keeps the engine warm during quick probe → real-connect
    /// sequences without locking the new client into a stale getstream `pid`.
    pub async fn release_client(&self, infohash: &str) {
        let prev_count = match self.sessions.get(infohash) {
            Some(s) => s.client_count.fetch_sub(1, Ordering::Relaxed),
            None => return,
        };
        // DashMap read-lock released here (guard dropped at end of `match`)

        if prev_count == 0 {
            // Underflow guard — shouldn't happen but AtomicUsize wraps on subtract.
            return;
        }

        if prev_count > 1 {
            tracing::debug!(
                "Acestream client released ({} remaining) for {infohash:.16}",
                prev_count - 1
            );
            return;
        }

        // Last client disconnected — remove the session from the map NOW so that
        // reconnecting clients always get a fresh session (new pid).
        let command_url = self
            .sessions
            .get(infohash)
            .and_then(|s| s.command_url.clone());
        self.sessions.remove(infohash);

        tracing::debug!(
            "Acestream last client released for {infohash:.16} — session removed, engine stops in 30 s"
        );

        let manager = self.clone();
        let infohash_owned = infohash.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if manager.sessions.contains_key(&infohash_owned) {
                // A new session was created during the grace period — it owns the engine now.
                tracing::debug!(
                    "Acestream grace period: new session active for {infohash_owned:.16}, skipping engine stop"
                );
            } else {
                tracing::info!(
                    "Acestream stopping idle engine for {infohash_owned:.16} after grace period"
                );
                manager
                    .send_stop_command(&infohash_owned, command_url)
                    .await;
            }
        });
    }

    /// Send a stop command to the engine for a session (forced, e.g. admin/invalidation).
    pub async fn stop_session(&self, infohash: &str) {
        let command_url = self
            .sessions
            .get(infohash)
            .and_then(|s| s.command_url.clone());

        self.sessions.remove(infohash);
        self.send_stop_command(infohash, command_url).await;
    }

    async fn send_stop_command(&self, infohash: &str, command_url: Option<String>) {
        if let Some(url) = command_url {
            let stop_url = if url.contains('?') {
                format!("{url}&method=stop")
            } else {
                format!("{url}?method=stop")
            };
            tracing::info!("Acestream: stopping session {infohash:.16} via {stop_url}");
            let _ = self
                .client
                .get(&stop_url)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        }
    }

    /// Invalidate a session without sending a stop command (e.g. after 403 from engine).
    pub fn invalidate(&self, infohash: &str) {
        self.sessions.remove(infohash);
    }
}

/// Returns true if the error string indicates the engine requires premium.
fn is_premium_error(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("premium") || lower.contains("activate") || lower.contains("subscription")
}

/// Returns true if `s` looks like a 40-character lowercase hex infohash.
fn is_infohash(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Rewrite the host:port in a URL returned by the acestream engine.
///
/// The engine always embeds its own view of its address (typically
/// `localhost:6878`) in `playback_url`, `stat_url` and `command_url`.
/// When the proxy is running on a different machine than the engine we must
/// replace those self-references with the configured `engine_host:engine_port`
/// so that subsequent HTTP calls actually reach the engine.
///
/// We replace any of the common self-referencing forms:
///   `localhost`, `127.0.0.1`, `::1`
fn rewrite_engine_host(url: String, engine_host: &str, engine_port: u16) -> String {
    // If the engine host is already localhost/loopback, nothing to rewrite.
    let host_is_local = matches!(engine_host, "localhost" | "127.0.0.1" | "::1");
    if host_is_local {
        return url;
    }

    // Replace any loopback authority in the URL with the actual engine address.
    // Handles both "localhost:PORT" and "127.0.0.1:PORT" patterns.
    let replacement = format!("{engine_host}:{engine_port}");
    url.replace(&format!("localhost:{engine_port}"), &replacement)
        .replace(&format!("127.0.0.1:{engine_port}"), &replacement)
        .replace(&format!("[::1]:{engine_port}"), &replacement)
}
