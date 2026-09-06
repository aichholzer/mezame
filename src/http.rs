//! Cloudflared transport: the HTTP/WS server that fronts Mezame.
//!
//! axum serves the embedded UI at `/` and accepts WS upgrades at `/ws`.
//! Public reachability is delegated to an external Cloudflare Tunnel;
//! Mezame binds loopback by default.
//!
//! Also home to the plain HTTP endpoints: `/state` (cross-device browser
//! state), `/history` (a session's transcript), and the embedded-asset
//! fallback.

use std::collections::HashMap;
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
    /// on this and end their futures promptly. Without it they
    /// would hold the serve loop open forever.
    pub shutdown: Arc<Notify>,
}

/// React UI bundle baked into the binary by `build.rs` + `rust-embed`.
///
/// The build script compiles the React/Vite app into
/// `$OUT_DIR/ui/dist/` and leaves the source directory untouched. That is
/// a hard crates.io requirement. `rust-embed`'s
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
    enable_tcp_keepalive(&listener);
    eprintln!("Mezame is listening on: http://{bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await?;
    Ok(())
}

/// Enable TCP keepalive on the listening socket as a kernel-level
/// backstop to the application heartbeat in `src/ws.rs`. Accepted
/// connections inherit the listener's keepalive setting on Linux. A
/// half-open socket the kernel can detect (no ACKs for the probes) is
/// eventually torn down even with the app-level ping task wedged. The app
/// heartbeat is the primary defence, and it also catches peers that ACK at
/// the TCP layer but have stopped reading. This keeps the kernel from
/// holding a truly dead socket `ESTABLISHED` forever. Best-effort: a
/// failure here is logged and ignored, and startup continues. See GitHub
/// issue #4.
///
/// Public for the integration tests in `tests/`. Its only caller is
/// `run_cloudflared`, which serves until a signal arrives.
pub fn enable_tcp_keepalive(listener: &TcpListener) {
    use socket2::{SockRef, TcpKeepalive};

    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(20));
    let sock = SockRef::from(listener);
    if let Err(e) = sock.set_tcp_keepalive(&keepalive) {
        eprintln!("Could not enable TCP keepalive on the listener: {e}");
    }
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
/// connections on the returned future. Mezame exits promptly when its
/// service manager asks it to.
///
/// Before returning we fire `shutdown`. Long-poll handlers in flight (the
/// SSE state-events stream) end their futures, and axum's graceful drain
/// completes. Without it the drain waits on them forever.
///
/// Live WebSocket sessions are dropped on shutdown. Each hub's Backend is
/// released with it, and its transcript goes: nothing on disk survives a
/// restart in this phase.
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
    // Wake every long-poll handler. They release their futures before
    // axum's drain kicks in. `notify_waiters` only wakes tasks that are
    // currently waiting; long-pollers attached after this point check the
    // same flag inline before subscribing.
    shutdown.notify_waiters();
}

/// Serve a single file from the embedded UI bundle.
///
/// Strips the leading `/` and falls back to `index.html` for empty paths
/// and for any unknown path, leaving the SPA to handle its own routing.
/// Sets a reasonable Cache-Control: long-lived for the hashed `/assets/*`
/// filenames Vite emits, no-cache for `index.html`.
async fn serve_ui_asset(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');
    // Resolve to an actual asset. `/` and unknown routes both fall back to
    // `index.html`, and the SPA handles its own routing from there.
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
        // Neither `index.html` nor the service-worker script tolerates
        // aggressive caching. `index.html` is the SPA entry point, and
        // `sw.js` is how the SW updates itself. Browsers already bypass
        // the HTTP cache for SW updates in most cases; the explicit
        // no-cache keeps any intermediary from stashing it.
        "no-cache, no-store, must-revalidate"
    } else if resolved_path.starts_with("assets/") {
        // Vite emits content-hashed filenames under /assets. A year of
        // caching cannot serve stale content.
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

/// GET /state: returns the persisted browser state as JSON, or `{}` if the
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

/// PUT /state: atomically replaces the stored state. Writes to a sibling
/// `.tmp` then calls `rename`, and readers never see a partial file. A
/// successful write fires a tick on the `state_changes` broadcast. Every
/// browser subscribed to `/state/events` then refetches and merges in any
/// new sessions another browser opened.
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
    // A send error here only means no browser is currently subscribed.
    // The next subscriber fetches /state on connect and sees everything
    // that changed in the meantime.
    let _ = app.state_changes.send(());
    Ok(StatusCode::NO_CONTENT)
}

/// GET /state/events: Server-Sent Events stream. Emits one
/// `state_changed` event each time `put_state` writes a new state
/// file. The browser reads it as a "go refetch /state" signal, and
/// sessions opened in another browser show up without a manual
/// reload.
///
/// A periodic keep-alive comment goes out alongside. A Cloudflare Tunnel
/// or other intermediary would otherwise idle-timeout the stream during a
/// quiet period.
async fn state_events(
    State(app): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = app.state_changes.subscribe();
    let shutdown = app.shutdown.clone();
    let stream = futures_util::stream::unfold((rx, shutdown), |(mut rx, shutdown)| async move {
        loop {
            tokio::select! {
                // Shutdown wins: end the stream and let axum's
                // graceful drain finish. Without this the SSE handler
                // holds a request future that never resolves, and
                // Ctrl+C hangs.
                _ = shutdown.notified() => return None,
                msg = rx.recv() => match msg {
                    Ok(()) => {
                        return Some((
                            Ok(Event::default().event("state_changed").data("")),
                            (rx, shutdown),
                        ));
                    }
                    // Lagged: skip and wait for the next message. The
                    // browser refetches on the next event delivered.
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

/// `GET /history?session=<id>`: the transcript of the Backend behind that
/// session id, as an `entries` array in recorded order with no cap and no
/// pagination.
///
/// An absent or empty `session` answers 400 with a plain-text body. An id
/// with no registered hub answers 200 with an empty array, which covers a
/// value holding `/`, `\` or `..` with no separate validation step: no
/// such value can be a registry key. Nothing here reads a file or
/// consults `HOME`, and the endpoint answers 200 or 400 and nothing else.
///
/// A transcript lives as long as its hub. A reload inside the grace window
/// shows the conversation so far; one after it shows an empty log.
async fn get_history(
    Query(params): Query<HashMap<String, String>>,
    State(app): State<Arc<AppState>>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let sid = params.get("session").map(String::as_str).unwrap_or("");
    if sid.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "missing ?session=<id>".into()));
    }
    let entries = app.hubs.history(sid).await.unwrap_or_default();
    Ok(Json(json!({ "entries": entries })))
}
