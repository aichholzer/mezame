//! Cloudflared transport: the HTTP/WS server that fronts Mezame.
//!
//! axum serves the embedded UI at `/` and accepts WS upgrades at `/ws`.
//! Public reachability is delegated to an external Cloudflare Tunnel;
//! Mezame binds loopback by default.
//!
//! Also home to the plain HTTP endpoints: `/state` (cross-device browser
//! state), `/history` (Kiro JSONL replay), and the embedded-asset fallback.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode, Uri},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use futures_util::stream::Stream;
use rust_embed::RustEmbed;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Notify};

use crate::config::{state_path, Config};
use crate::hub::HubRegistry;
use crate::ws::ws_upgrade;

/// Shared state for the axum router. Bundles the static `Config` with
/// the live `HubRegistry` so the WS handler can attach to existing
/// hubs or create new ones for fresh sessions, plus a broadcast
/// channel that fires whenever `state.json` is rewritten so connected
/// browsers can re-sync their session list without a manual reload.
pub struct AppState {
    pub config: Arc<Config>,
    pub hubs: HubRegistry,
    /// Tick channel: `put_state` fires `()` on every successful
    /// rename. Browsers subscribed to `/state/events` receive an
    /// SSE event and refetch `/state`. Receivers that lag behind
    /// are dropped silently; the next tick brings them back in
    /// sync. Capacity 64 is plenty given state writes happen at
    /// human-edit pace.
    pub state_changes: broadcast::Sender<()>,
    /// Process-wide shutdown signal. Fired by the SIGINT/SIGTERM
    /// handler before letting axum's graceful shutdown drain.
    /// Long-poll handlers (currently just the SSE stream) listen
    /// on this so they end their futures promptly instead of
    /// holding the serve loop hostage forever.
    pub shutdown: Arc<Notify>,
}

/// React UI bundle baked into the binary by `build.rs` + `rust-embed`.
///
/// The build script compiles the React/Vite app into
/// `$OUT_DIR/ui/dist/` so the source directory stays untouched, which
/// is a hard crates.io requirement. `rust-embed`'s
/// `interpolate-folder-path` feature lets us reference `$OUT_DIR` in the
/// attribute below.
#[derive(RustEmbed)]
#[folder = "$OUT_DIR/ui/dist/"]
struct UiAssets;

// TODO(auth): validate the `Cf-Access-Jwt-Assertion` header on /ws before
// allowing the upgrade. The header is injected by Cloudflare Access; its
// signing keys are at
//   https://<team>.cloudflareaccess.com/cdn-cgi/access/certs

pub(crate) async fn run_cloudflared(cfg: Config, bind: String) -> Result<()> {
    let (state_changes, _) = broadcast::channel(64);
    let shutdown = Arc::new(Notify::new());
    let state = Arc::new(AppState {
        config: Arc::new(cfg),
        hubs: HubRegistry::new(),
        state_changes,
        shutdown: shutdown.clone(),
    });
    let app = build_router(state);

    let listener = TcpListener::bind(&bind).await?;
    eprintln!("Mezame is listening on: http://{bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
}

/// Construct the axum router with all production routes wired in. Split
/// out from `run_cloudflared` so integration tests can drive it via
/// `tower::ServiceExt::oneshot` without binding a TCP port.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/state", get(get_state).put(put_state))
        .route("/state/events", get(state_events))
        .route("/history", get(get_history))
        // SPA fallback: /, /assets/*, and any unknown path resolve against
        // the embedded UI bundle, with index.html as the fallback for
        // client-side routes.
        .fallback(get(serve_ui_asset))
        .with_state(state)
}

/// Resolve when the process receives SIGINT (Ctrl+C) or SIGTERM (systemd
/// / launchd `stop`). `with_graceful_shutdown` stops accepting new
/// connections on the returned future, so Mezame exits promptly when its
/// service manager asks it to.
///
/// Before returning we fire `shutdown` so any long-poll handlers in
/// flight (the SSE state-events stream) end their futures and let
/// axum's graceful drain complete instead of waiting on them forever.
///
/// Live WebSocket sessions are dropped on shutdown; the agent subprocess
/// is killed (`kill_on_drop`), which may leave a Kiro session lockfile
/// behind. The next start self-heals via `steal_stale_session_lock`.
async fn shutdown_signal(shutdown: Arc<Notify>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                eprintln!("Failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => eprintln!("\nReceived SIGINT, shutting down."),
        _ = terminate => eprintln!("Received SIGTERM, shutting down."),
    }
    // Wake every long-poll handler so they release their futures
    // before axum's drain kicks in. `notify_waiters` only wakes
    // tasks that are currently waiting; long-pollers attached after
    // this point check the same flag inline before subscribing.
    shutdown.notify_waiters();
}

/// Serve a single file from the embedded UI bundle.
///
/// Strips the leading `/`, falls back to `index.html` for empty paths and
/// for any unknown path (so the SPA handles its own routing). Sets a
/// reasonable Cache-Control: long-lived for hashed `/assets/*` filenames
/// Vite emits, no-cache for `index.html`.
async fn serve_ui_asset(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');
    // Resolve to an actual asset. `/` and unknown routes both fall back to
    // `index.html` so the SPA can handle its own routing.
    let (asset, resolved_path) = match UiAssets::get(raw_path) {
        Some(a) => (a, raw_path),
        None => match UiAssets::get("index.html") {
            Some(a) => (a, "index.html"),
            None => {
                return (StatusCode::NOT_FOUND, "UI bundle missing").into_response();
            }
        },
    };
    let is_index = resolved_path == "index.html";

    let mime = mime_for(resolved_path);
    let cache_control = if is_index || resolved_path == "sw.js" {
        // Both `index.html` and the service-worker script must not be
        // cached aggressively: `index.html` is the SPA entry point and
        // `sw.js` is how we update the SW itself (browsers already
        // bypass HTTP cache for SW updates in most cases, but the
        // explicit no-cache keeps any intermediary from stashing it).
        "no-cache, no-store, must-revalidate"
    } else if resolved_path.starts_with("assets/") {
        // Vite emits content-hashed filenames under /assets, so we can
        // cache them for a year without risking stale content.
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, HeaderValue::from_static(mime))
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        )
        .body(Body::from(asset.data.into_owned()))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "response build failed").into_response()
        })
}

/// Tiny mime-type lookup for the handful of extensions Vite emits. Keeps us
/// off a `mime_guess` dependency. The const table is the single source of
/// truth; matching is case-insensitive without allocating a lowercase copy
/// of the extension on every request.
const MIME_TABLE: &[(&str, &str)] = &[
    ("html", "text/html; charset=utf-8"),
    ("js", "application/javascript; charset=utf-8"),
    ("mjs", "application/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("json", "application/json; charset=utf-8"),
    ("map", "application/json; charset=utf-8"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("txt", "text/plain; charset=utf-8"),
    ("webmanifest", "application/manifest+json"),
];

pub fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("");
    MIME_TABLE
        .iter()
        .find(|(k, _)| ext.eq_ignore_ascii_case(k))
        .map(|(_, v)| *v)
        .unwrap_or("application/octet-stream")
}

/// GET /state — returns the persisted browser state as JSON, or `{}` if the
/// file does not exist yet. Mezame does not interpret the contents; it is
/// purely a cross-device store for the UI.
async fn get_state() -> Result<Json<Value>, (StatusCode, String)> {
    let path = state_path().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    match tokio::fs::read_to_string(&path).await {
        Ok(raw) => {
            let v: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
            Ok(Json(v))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Json(json!({}))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))),
    }
}

/// PUT /state — atomically replaces the stored state. Writes to a sibling
/// `.tmp` then `rename` so readers never see a partial file. After a
/// successful write we fire a tick on the `state_changes` broadcast so
/// every browser subscribed to `/state/events` knows to refetch and
/// merge in any new sessions another browser opened.
async fn put_state(
    State(app): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<StatusCode, (StatusCode, String)> {
    let path = state_path().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(&body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    tokio::fs::write(&tmp, data)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    // Send-error here only means no browser is currently subscribed,
    // which is fine: the next subscriber will fetch /state on connect
    // and pick up any changes since.
    let _ = app.state_changes.send(());
    Ok(StatusCode::NO_CONTENT)
}

/// GET /state/events — Server-Sent Events stream. Emits one
/// `state_changed` event each time `put_state` writes a new state
/// file. The browser uses this as a "go refetch /state" signal so
/// new sessions opened in another browser show up without a manual
/// reload.
///
/// We also emit a periodic keep-alive comment so a Cloudflare Tunnel
/// or other intermediary does not idle-timeout the stream during a
/// quiet period.
async fn state_events(
    State(app): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = app.state_changes.subscribe();
    let shutdown = app.shutdown.clone();
    let stream = futures_util::stream::unfold((rx, shutdown), |(mut rx, shutdown)| async move {
        loop {
            tokio::select! {
                // Shutdown wins: end the stream so axum's graceful
                // drain can finish. Without this, Ctrl+C hangs
                // because the SSE handler holds a request future
                // that never resolves.
                _ = shutdown.notified() => return None,
                msg = rx.recv() => match msg {
                    Ok(()) => {
                        return Some((
                            Ok(Event::default().event("state_changed").data("")),
                            (rx, shutdown),
                        ));
                    }
                    // Lagged: skip and wait for the next message. The
                    // browser will refetch on the next event we deliver.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    // All senders dropped: end the stream. In practice
                    // this only happens when the server is shutting down.
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
            }
        }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

/// GET /history?session=<id> — returns a compact history reconstructed from
/// Kiro's own `~/.kiro/sessions/cli/<id>.jsonl` event log.
///
/// Kiro records a `meta.timestamp` (Unix seconds) only on `Prompt` entries.
/// Subsequent `AssistantMessage` / `ToolResults` inherit the timestamp of
/// the most recent preceding `Prompt`, which is the right grouping: a
/// turn and its reply share a single user-facing time.
///
/// Returned JSON:
///   { "entries": [{ "role": "user"|"agent"|"sys", "text": "...",
///                   "timestamp": <ms since epoch> }, ...] }
///
/// Missing session file → `{ "entries": [] }`, not an error. Reading a
/// file Kiro currently has open for append is safe; we only read.
async fn get_history(
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let Some(sid) = params.get("session") else {
        return Err((StatusCode::BAD_REQUEST, "missing ?session=<id>".into()));
    };
    // Block path traversal defensively: Kiro session ids are UUIDs, and
    // we only ever want a single file next to the others.
    if sid.is_empty() || sid.contains('/') || sid.contains('\\') || sid.contains("..") {
        return Err((StatusCode::BAD_REQUEST, "invalid session id".into()));
    }
    let Ok(home) = std::env::var("HOME") else {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "HOME not set".into()));
    };
    let path = PathBuf::from(home).join(format!(".kiro/sessions/cli/{sid}.jsonl"));
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Json(json!({ "entries": [] })));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}"))),
    };

    let entries = parse_kiro_history(&raw);
    Ok(Json(json!({ "entries": entries })))
}

/// Parse Kiro's session JSONL into compact browser-facing entries.
///
/// Shape we consume (all other variants ignored):
///   { "kind": "Prompt", "data": {
///       "content": [{ "kind": "text", "data": "..." }, ...],
///       "meta": { "timestamp": <unix seconds> } } }
///   { "kind": "AssistantMessage", "data": {
///       "content": [{ "kind": "text", "data": "..." }, ...] } }
///
/// Any `content` block whose `kind` is not `"text"` is dropped here; we
/// don't try to reconstruct thinking blocks, tool calls, or tool results
/// in the history view. If Kiro ever starts emitting verbose tool panels
/// in the UI, those should come through as structured events separately.
pub fn parse_kiro_history(raw: &str) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    // Timestamp of the most recent Prompt. Persisted in ms for the
    // browser's `Date` math; Kiro stores seconds.
    let mut current_ts_ms: Option<i64> = None;

    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = entry.get("kind").and_then(Value::as_str).unwrap_or("");
        let data = entry.get("data").cloned().unwrap_or(Value::Null);

        match kind {
            "Prompt" => {
                let ts_sec = data
                    .get("meta")
                    .and_then(|m| m.get("timestamp"))
                    .and_then(Value::as_i64);
                if let Some(secs) = ts_sec {
                    current_ts_ms = Some(secs.saturating_mul(1000));
                }
                if let Some(text) = extract_text_blocks(&data) {
                    out.push(json!({
                        "role": "user",
                        "text": text,
                        "timestamp": current_ts_ms
                    }));
                }
            }
            "AssistantMessage" => {
                if let Some(text) = extract_text_blocks(&data) {
                    out.push(json!({
                        "role": "agent",
                        "text": text,
                        "timestamp": current_ts_ms
                    }));
                }
            }
            _ => {
                // Ignore ToolResults, thinking-only messages, any other
                // variants. The live view will render those for new
                // turns; replayed history stays lean.
            }
        }
    }

    out
}

/// Concatenate all `content[].data` strings where `content[].kind == "text"`,
/// with newlines between blocks. Returns `None` when there's nothing useful
/// (e.g. an assistant turn that was only a tool call).
pub fn extract_text_blocks(data: &Value) -> Option<String> {
    let content = data.get("content")?.as_array()?;
    let mut buf = String::new();
    for block in content {
        if block.get("kind").and_then(Value::as_str) == Some("text") {
            if let Some(s) = block.get("data").and_then(Value::as_str) {
                if !s.is_empty() {
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(s);
                }
            }
        }
    }
    if buf.is_empty() {
        None
    } else {
        Some(buf)
    }
}
