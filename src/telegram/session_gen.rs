//! HTTP endpoints that implement the Telegram login flow the web UI drives:
//!
//!   POST /proxy/telegram/session/start    — send login code (phone) or
//!                                           finalise bot auth
//!   POST /proxy/telegram/session/verify   — exchange SMS code for a session
//!   POST /proxy/telegram/session/2fa      — submit cloud-password when 2FA is on
//!   POST /proxy/telegram/session/cancel   — abort a pending phone flow
//!
//! The multi-step phone flow is stored in a process-local `DashMap` keyed by
//! an opaque random `session_id`; each entry holds the live grammers `Client`
//! and the in-flight `LoginToken` (or `PasswordToken` after 2FA is triggered).
//! Entries are reaped after [`SESSION_TTL_SECS`].
//!
//! On success the endpoints return a Telethon-compatible `StringSession`
//! ("1" + base64url(dc_id | ipv4 | port_be | auth_key_256)), the same format
//! the legacy `parse_telethon_session` already consumes — so users can paste
//! the output straight into `APP__TELEGRAM__SESSION_STRING`.

#![cfg(feature = "telegram")]

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use actix_web::{web, HttpResponse};
use base64::{engine::general_purpose, Engine};
use dashmap::DashMap;
use grammers_client::session::Session;
use grammers_client::types::{LoginToken, PasswordToken};
use grammers_client::{Client, Config, InitParams, SignInError};
use rand::RngCore;
use serde::Deserialize;
use tracing::{info, warn};

// Errors are emitted as bare HttpResponse JSON bodies with a `detail` field so
// they match the web UI's error handler (which mirrors FastAPI's
// `HTTPException(detail=…)` shape — `data.detail || fallback`).

const SESSION_TTL_SECS: u64 = 600; // 10 minutes

// ---------------------------------------------------------------------------
// Pending-session store
// ---------------------------------------------------------------------------

struct PendingSession {
    client: Arc<Client>,
    api_id: i32,
    api_hash: String,
    login_token: Option<LoginToken>,
    password_token: Option<PasswordToken>,
    created_at: Instant,
}

fn pending_map() -> &'static DashMap<String, PendingSession> {
    static M: OnceLock<DashMap<String, PendingSession>> = OnceLock::new();
    M.get_or_init(DashMap::new)
}

fn gc_expired() {
    let now = Instant::now();
    pending_map()
        .retain(|_, s| now.duration_since(s.created_at) < Duration::from_secs(SESSION_TTL_SECS));
}

fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Request / response types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SessionStartRequest {
    api_id: i32,
    api_hash: String,
    /// "phone" or "bot".
    auth_type: String,
    phone: Option<String>,
    bot_token: Option<String>,
}

#[derive(Deserialize)]
pub struct SessionVerifyRequest {
    session_id: String,
    code: String,
}

#[derive(Deserialize)]
pub struct Session2FaRequest {
    session_id: String,
    password: String,
}

#[derive(Deserialize)]
pub struct SessionCancelRequest {
    session_id: String,
}

// ---------------------------------------------------------------------------
// Session-string encoder
// ---------------------------------------------------------------------------

/// Encode the authenticated client's session as a Telethon
/// [`StringSession`](https://docs.telethon.dev/en/latest/modules/sessions.html#module-telethon.sessions.string).
///
/// Layout: `'1' | base64url( dc_id u8 | ipv4 4B | port u16 BE | auth_key 256B )`
fn export_telethon_string(client: &Client) -> Option<String> {
    let session = client.session();
    let user = session.get_user()?;
    let dc_id = user.dc;
    let dc = session.get_dcs().into_iter().find(|d| d.id == dc_id)?;
    let auth_key = session.dc_auth_key(dc_id)?;

    // grammers stores the IPv4 as `i32::from_le_bytes(octets)` (see
    // Session::insert_dc) — inverting with `to_le_bytes` recovers the
    // dotted-octet order used by Telethon.
    let ipv4 = dc.ipv4?;
    let octets = ipv4.to_le_bytes();
    let port = dc.port as u16;

    let mut buf = Vec::with_capacity(263);
    buf.push(dc_id as u8);
    buf.extend_from_slice(&octets);
    buf.extend_from_slice(&port.to_be_bytes());
    buf.extend_from_slice(&auth_key);

    Some(format!(
        "1{}",
        general_purpose::URL_SAFE_NO_PAD.encode(&buf)
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn build_client(api_id: i32, api_hash: &str) -> Result<Arc<Client>, String> {
    Client::connect(Config {
        session: Session::new(),
        api_id,
        api_hash: api_hash.to_string(),
        params: InitParams {
            catch_up: false,
            ..Default::default()
        },
    })
    .await
    .map(Arc::new)
    .map_err(|e| format!("{e}"))
}

fn bad_request(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(serde_json::json!({ "detail": msg.into() }))
}

fn not_found(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::NotFound().json(serde_json::json!({ "detail": msg.into() }))
}

fn internal_error(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::InternalServerError().json(serde_json::json!({ "detail": msg.into() }))
}

/// Map Telegram RPC error text to a user-friendly 400 detail.
fn friendly_rpc_error(context: &str, err: &dyn std::fmt::Display) -> HttpResponse {
    let text = err.to_string();
    let upper = text.to_uppercase();
    let detail = if upper.contains("PHONE_NUMBER_INVALID") {
        "Invalid phone number format. Use international format (e.g. +1234567890).".to_string()
    } else if upper.contains("PHONE_NUMBER_BANNED") {
        "This phone number is banned from Telegram.".to_string()
    } else if upper.contains("FLOOD") {
        "Too many attempts. Wait before retrying.".to_string()
    } else {
        format!("{context}: {text}")
    };
    warn!("telegram session-gen error [{context}]: {text}");
    bad_request(detail)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn session_start_handler(req: web::Json<SessionStartRequest>) -> HttpResponse {
    gc_expired();

    if req.api_id <= 0 || req.api_hash.is_empty() {
        return bad_request("api_id and api_hash are required");
    }

    let client = match build_client(req.api_id, &req.api_hash).await {
        Ok(c) => c,
        Err(e) => return internal_error(format!("Client::connect: {e}")),
    };

    match req.auth_type.as_str() {
        "bot" => {
            let token = req.bot_token.as_deref().unwrap_or("").trim().to_string();
            if token.is_empty() {
                return bad_request("bot_token is required for bot authentication");
            }

            match client.bot_sign_in(&token).await {
                Ok(_user) => {
                    let session_string = match export_telethon_string(&client) {
                        Some(s) => s,
                        None => {
                            return internal_error("Signed in but could not export session string")
                        }
                    };
                    info!("Telegram bot session generated (api_id={})", req.api_id);
                    HttpResponse::Ok().json(serde_json::json!({
                        "success":      true,
                        "step":         "complete",
                        "session_string": session_string,
                        "api_id":       req.api_id,
                        "api_hash":     req.api_hash,
                    }))
                }
                Err(e) => friendly_rpc_error("Bot authentication failed", &e),
            }
        }

        "phone" => {
            let phone = req.phone.as_deref().unwrap_or("").trim().to_string();
            if phone.is_empty() {
                return bad_request("phone is required for phone authentication");
            }

            let login_token = match client.request_login_code(&phone).await {
                Ok(t) => t,
                Err(e) => return friendly_rpc_error("Failed to send login code", &e),
            };

            let session_id = new_session_id();
            pending_map().insert(
                session_id.clone(),
                PendingSession {
                    client,
                    api_id: req.api_id,
                    api_hash: req.api_hash.clone(),
                    login_token: Some(login_token),
                    password_token: None,
                    created_at: Instant::now(),
                },
            );

            info!("Telegram login code sent (session_id={})", &session_id[..8]);
            HttpResponse::Ok().json(serde_json::json!({
                "success":    true,
                "session_id": session_id,
                "step":       "code_sent",
                "message":    "Verification code sent to your Telegram app",
            }))
        }

        other => bad_request(format!(
            "Unknown auth_type '{other}'; must be 'phone' or 'bot'"
        )),
    }
}

pub async fn session_verify_handler(req: web::Json<SessionVerifyRequest>) -> HttpResponse {
    // Take the pending session out so only one verify can run at a time per id.
    let (_key, mut pending) = match pending_map().remove(&req.session_id) {
        Some(p) => p,
        None => {
            return not_found("Session not found or expired. Please start again.");
        }
    };

    let token = match pending.login_token.take() {
        Some(t) => t,
        None => {
            // Put the (now token-less) pending back so the user can submit
            // a 2FA password instead of being forced to restart.
            pending_map().insert(req.session_id.clone(), pending);
            return bad_request(
                "No code to verify on this session. If 2FA is required, submit the password step instead.",
            );
        }
    };

    match pending.client.sign_in(&token, &req.code).await {
        Ok(_user) => {
            let session_string = match export_telethon_string(&pending.client) {
                Some(s) => s,
                None => return internal_error("Signed in but could not export session string"),
            };
            info!(
                "Telegram phone session generated (api_id={})",
                pending.api_id
            );
            HttpResponse::Ok().json(serde_json::json!({
                "success":        true,
                "step":           "complete",
                "session_string": session_string,
                "api_id":         pending.api_id,
                "api_hash":       pending.api_hash,
            }))
        }
        Err(SignInError::PasswordRequired(pwd_token)) => {
            let hint = pwd_token.hint().map(|s| s.to_string());
            // Put the session back so the 2FA step can pick it up.
            pending.password_token = Some(pwd_token);
            pending.created_at = Instant::now(); // reset TTL
            let session_id = req.session_id.clone();
            pending_map().insert(session_id.clone(), pending);

            // UI (static/url_generator.html line ~2986) switches on
            // `data.step === "2fa_required"` — keep the string exact.
            HttpResponse::Ok().json(serde_json::json!({
                "success":    true,
                "step":       "2fa_required",
                "session_id": session_id,
                "hint":       hint,
                "message":    "Two-factor password required",
            }))
        }
        Err(SignInError::InvalidCode) => {
            // Put the session back so the user can retry the code.
            pending.login_token = Some(token);
            pending.created_at = Instant::now();
            pending_map().insert(req.session_id.clone(), pending);
            bad_request("Invalid verification code")
        }
        Err(SignInError::SignUpRequired { .. }) => bad_request(
            "This phone number has no Telegram account. Sign up via the official app first.",
        ),
        Err(e) => friendly_rpc_error("Sign-in failed", &e),
    }
}

pub async fn session_2fa_handler(req: web::Json<Session2FaRequest>) -> HttpResponse {
    let (_key, mut pending) = match pending_map().remove(&req.session_id) {
        Some(p) => p,
        None => return not_found("Session not found or expired. Please start again."),
    };

    let pwd_token = match pending.password_token.take() {
        Some(t) => t,
        None => {
            return bad_request("No 2FA challenge pending on this session; verify a code first.")
        }
    };

    match pending
        .client
        .check_password(pwd_token, req.password.as_bytes())
        .await
    {
        Ok(_user) => {
            let session_string = match export_telethon_string(&pending.client) {
                Some(s) => s,
                None => return internal_error("Signed in but could not export session string"),
            };
            info!(
                "Telegram phone+2FA session generated (api_id={})",
                pending.api_id
            );
            HttpResponse::Ok().json(serde_json::json!({
                "success":        true,
                "step":           "complete",
                "session_string": session_string,
                "api_id":         pending.api_id,
                "api_hash":       pending.api_hash,
            }))
        }
        Err(e) => friendly_rpc_error("2FA password check failed", &e),
    }
}

pub async fn session_cancel_handler(req: web::Json<SessionCancelRequest>) -> HttpResponse {
    // Simply drop the pending session; grammers' Client teardown happens on
    // Arc::drop since nothing else holds a reference.
    let removed = pending_map().remove(&req.session_id).is_some();
    HttpResponse::Ok().json(serde_json::json!({
        "success": true,
        "cancelled": removed,
    }))
}
