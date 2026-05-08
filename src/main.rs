//! Okiro: an ACP client that bridges a local agent to a browser UI.
//!
//! One WebSocket connection = one agent subprocess = one ACP session.
//! The agent is killed when the browser disconnects (`kill_on_drop(true)`).
//!
//! See the README for architecture, wire protocol, transports, and
//! extension points. In-code extension points are marked with `TODO:`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    body::Body,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State
    },
    http::{header, HeaderValue, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router
};
use futures_util::{SinkExt, StreamExt};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

const PROTOCOL_VERSION: u32 = 1;

/// React UI bundle baked into the binary by `build.rs` + `rust-embed`.
///
/// In `--release` the files are compiled in and the binary is truly
/// self-contained. In debug builds (with the `debug-embed` feature) files
/// are read from disk at runtime, so a `npm run build` in `ui/` is enough
/// to refresh the bundle without touching cargo.
#[derive(RustEmbed)]
#[folder = "ui/dist/"]
struct UiAssets;

// ---------- config ----------
//
// On-disk config lives at `~/.okiro/config.toml`. Schema changes are breaking
// for existing users, so add fields with `#[serde(default)]` rather than
// reshuffling. The `Transport` enum gates which `run_*` function main() calls.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    transport: Transport,
    bind: String,
    agent_cmd: String,
    #[serde(default)]
    agent_args: Vec<String>,
    #[serde(default)]
    telegram: TelegramConfig
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Transport {
    Cloudflared,
    Telegram
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TelegramConfig {
    #[serde(default)]
    token: String
}

fn config_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".okiro/config.toml"))
}

/// Path to the persistent browser state (currently-open tabs, history list,
/// active id, next numeric label). Server-side so any device hitting Okiro
/// sees the same list.
fn state_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".okiro/state.json"))
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).context("parsing config.toml")?;
    Ok(cfg)
}

fn prompt_line(msg: &str) -> Result<String> {
    use std::io::{BufRead, Write};
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn init_config() -> Result<Config> {
    println!();
    println!("Which transport?");
    println!("  1) cloudflared  (serve a terminal-like web UI; front with your tunnel)");
    println!("  2) telegram     (long-poll a Telegram bot)  [not yet implemented]");
    let transport = loop {
        match prompt_line("> ")?.as_str() {
            "1" | "cloudflared" => break Transport::Cloudflared,
            "2" | "telegram" => break Transport::Telegram,
            _ => println!("pick 1 or 2")
        }
    };

    let bind = {
        let s = prompt_line("bind address [127.0.0.1:7842]: ")?;
        if s.is_empty() {
            "127.0.0.1:7842".to_string()
        } else {
            s
        }
    };

    let agent_cmd = {
        let s = prompt_line("ACP agent command (e.g. kiro-cli, claude, gemini, codex): ")?;
        if s.is_empty() {
            bail!("agent command is required");
        }
        s
    };

    let args_raw = prompt_line("agent args (space-separated) []: ")?;
    let agent_args: Vec<String> = args_raw.split_whitespace().map(str::to_string).collect();

    let telegram = if transport == Transport::Telegram {
        let token = prompt_line("telegram bot token: ")?;
        TelegramConfig { token }
    } else {
        TelegramConfig::default()
    };

    let cfg = Config {
        transport,
        bind,
        agent_cmd,
        agent_args,
        telegram
    };

    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&cfg)?)?;
    println!("wrote {}", path.display());
    println!();
    Ok(cfg)
}

// ---------- entry ----------
//
// Kept synchronous on purpose: init_config() reads stdin and we do not want a
// tokio runtime blocking a thread on that. We only build the runtime once we
// know which transport to run.

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(String::as_str);

    if sub == Some("init") {
        init_config()?;
        return Ok(());
    }

    let cfg = if config_path()?.exists() {
        load_config()?
    } else {
        eprintln!("no config at {}", config_path()?.display());
        eprintln!("let's set one up:");
        init_config()?
    };

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        match cfg.transport {
            Transport::Cloudflared => run_cloudflared(cfg).await,
            Transport::Telegram => run_telegram(cfg).await
        }
    })
}

// ---------- cloudflared transport ----------
//
// axum serves the embedded UI at `/` and accepts WS upgrades at `/ws`. We bind
// on loopback only; public reachability is the Cloudflare Tunnel's job.
//
// TODO(auth): validate the `Cf-Access-Jwt-Assertion` header on /ws before
// allowing the upgrade. The header is injected by Cloudflare Access; its
// signing keys are at
//   https://<team>.cloudflareaccess.com/cdn-cgi/access/certs

async fn run_cloudflared(cfg: Config) -> Result<()> {
    let shared = Arc::new(cfg.clone());
    let app = Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/state", get(get_state).put(put_state))
        .route("/history", get(get_history))
        // SPA fallback: /, /assets/*, and any unknown path resolve against
        // the embedded UI bundle, with index.html as the fallback for
        // client-side routes.
        .fallback(get(serve_ui_asset))
        .with_state(shared);

    let listener = TcpListener::bind(&cfg.bind).await?;
    eprintln!("Okiro listening on http://{} (front with your Cloudflare Tunnel)", cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
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
        }
    };
    let is_index = resolved_path == "index.html";

    let mime = mime_for(resolved_path);
    let cache_control = if is_index {
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
        .header(header::CACHE_CONTROL, HeaderValue::from_static(cache_control))
        .body(Body::from(asset.data.into_owned()))
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "response build failed").into_response())
}

/// Tiny mime-type lookup for the handful of extensions Vite emits. Keeps us
/// off a `mime_guess` dependency.
fn mime_for(path: &str) -> &'static str {
    let lower = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match lower.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream"
    }
}

/// GET /state — returns the persisted browser state as JSON, or `{}` if the
/// file does not exist yet. Okiro does not interpret the contents; it is
/// purely a cross-device store for the UI.
async fn get_state() -> Result<Json<Value>, (StatusCode, String)> {
    let path = state_path().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let v: Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
            Ok(Json(v))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Json(json!({}))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))
    }
}

/// PUT /state — atomically replaces the stored state. Writes to a sibling
/// `.tmp` then `rename` so readers never see a partial file.
async fn put_state(Json(body): Json<Value>) -> Result<StatusCode, (StatusCode, String)> {
    let path = state_path().map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_string_pretty(&body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    std::fs::write(&tmp, data)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))?;
    Ok(StatusCode::NO_CONTENT)
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
    Query(params): Query<HashMap<String, String>>
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
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Json(json!({ "entries": [] })));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")))
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
fn parse_kiro_history(raw: &str) -> Vec<Value> {
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
fn extract_text_blocks(data: &Value) -> Option<String> {
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

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(cfg): State<Arc<Config>>
) -> Response {
    // `/ws?session=<acp-session-id>` asks Okiro to call `session/load` on the
    // agent instead of `session/new`. Absent = always new session.
    // `/ws?cwd=<path>` overrides the working directory for this session;
    // absent or empty = Okiro's own process cwd.
    let resume = params.get("session").cloned();
    let cwd_override = params
        .get("cwd")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = handle_ws(socket, cfg, resume, cwd_override).await {
            eprintln!("ws session ended: {e:?}");
        }
    })
}

/// Per-WebSocket session.
///
/// Ownership and concurrency:
///
/// - The WS is split into a `sink` (owned by a writer task) and a `stream`
///   (polled directly in the select loop).
/// - Sends to the browser go through an unbounded mpsc so handlers never
///   contend on the sink directly.
/// - The agent subprocess is spawned once per session and wrapped in `Arc`
///   so both the select loop and spawned prompt tasks can call into it.
/// - Prompts are run in their own spawned tasks so a long-running
///   `session/prompt` does not block the select loop from draining
///   `session/update` notifications.
async fn handle_ws(
    ws: WebSocket,
    cfg: Arc<Config>,
    resume_session_id: Option<String>,
    cwd_override: Option<String>
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();
    let (to_ws_tx, mut to_ws_rx) = mpsc::unbounded_channel::<Message>();

    // Writer task: drain the outbound channel into the WS sink. Exits when
    // the channel is closed (all senders dropped) or the sink errors.
    let writer = tokio::spawn(async move {
        while let Some(msg) = to_ws_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // The browser's sticky status banner shows "connecting..." until the
    // `ready` message arrives, so we no longer echo a startup line into
    // the log. This keeps the log free of protocol chatter.

    // If spawn fails (bad agent_cmd, missing binary, ...) tell the browser
    // and close cleanly. Do NOT return an Err here: the writer task still
    // needs to drain the error message before we exit.
    let agent = match spawn_agent(&cfg).await {
        Ok(a) => Arc::new(a),
        Err(e) => {
            let _ = to_ws_tx.send(text_msg(json!({ "type": "error", "message": format!("{e}") })));
            drop(to_ws_tx);
            let _ = writer.await;
            return Ok(());
        }
    };

    // ACP handshake. `initialize` advertises no filesystem capabilities
    // because Okiro does not back `fs/read_text_file` etc. today; the agent
    // is expected to use its own tools for file I/O.
    agent
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false }
                }
            })
        )
        .await
        .context("initialize")?;

    // Session setup. If the browser supplied a resume id, try `session/load`
    // first; on failure fall back to `session/new`. ACP's session/load
    // replays past messages via session/update notifications on the same
    // stream, which the select loop will forward to the browser as the
    // history rehydrates.
    //
    // `cwd` comes from the browser's `?cwd=<path>` query param if provided;
    // otherwise we use Okiro's own process cwd.
    let cwd_str = match cwd_override {
        Some(c) => c,
        None => std::env::current_dir()?.to_string_lossy().to_string()
    };

    let (session_id, resumed, session_info) = match resume_session_id {
        Some(sid) => match try_load_session(&agent, &sid, &cwd_str).await {
            Ok(value) => (sid, true, extract_session_info(&value)),
            Err(err_str) => {
                eprintln!("session/load failed ({err_str}); falling back to session/new");
                let _ = to_ws_tx.send(text_msg(json!({
                    "type": "append",
                    "role": "sys",
                    "text": format!(
                        "\n[previous session {} could not be resumed ({}). Starting a new one.]\n",
                        short_id(&sid),
                        short_reason(&err_str)
                    )
                })));
                let new_session = agent
                    .request(
                        "session/new",
                        json!({ "cwd": cwd_str, "mcpServers": [] })
                    )
                    .await
                    .context("session/new (fallback)")?;
                let sid = new_session
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("session/new returned no sessionId"))?
                    .to_string();
                (sid, false, extract_session_info(&new_session))
            }
        },
        None => {
            let new_session = agent
                .request(
                    "session/new",
                    json!({ "cwd": cwd_str, "mcpServers": [] })
                )
                .await
                .context("session/new")?;
            let sid = new_session
                .get("sessionId")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("session/new returned no sessionId"))?
                .to_string();
            (sid, false, extract_session_info(&new_session))
        }
    };

    // Tell the browser which session id it is bound to so it can persist it
    // for reconnect, and whether this was a resume (so it can clear stale
    // log before the replay lands).
    let _ = to_ws_tx.send(text_msg(json!({
        "type": "ready",
        "sessionId": session_id,
        "resumed": resumed
    })));

    // Send the `modes` and `models` payload (if present in either
    // session/new or session/load result) so the UI can render its
    // mode/model selectors and the current selections.
    if let Some(info) = session_info {
        let _ = to_ws_tx.send(text_msg(json!({
            "type": "session_info",
            "info": info
        })));
    }

    // After a successful `session/load`, Kiro replays past messages via
    // `session/update` notifications. The browser instead fetches the
    // history via `/history` (with real per-turn timestamps from the
    // on-disk `.jsonl`), so forwarding Kiro's replay would produce
    // duplicates. Drop `session/update` events until the first user-sent
    // prompt after resume; permission/tool requests still flow through.
    let mut suppress_session_updates = resumed;

    // Hand over the single updates receiver produced by `spawn_agent`. Only
    // one task may own it; we own it here, for the life of the session.
    let mut updates_rx = agent.take_updates();

    loop {
        tokio::select! {
            // User → agent: messages from the browser.
            Some(Ok(msg)) = stream.next() => {
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue
                };
                let v: Value = match serde_json::from_str(&text) {
                    Ok(v) => v,
                    Err(_) => continue
                };

                match v.get("type").and_then(Value::as_str) {
                    Some("prompt") => {
                        let Some(user_text) = v.get("text").and_then(Value::as_str) else {
                            continue;
                        };

                        // First live prompt after resume: stop hiding
                        // `session/update` events. From here on everything
                        // the agent emits is genuinely new.
                        suppress_session_updates = false;

                        // Run `session/prompt` in its own task so the select
                        // loop keeps pumping `session/update` notifications
                        // while the agent is working. When the request
                        // resolves we tell the browser the turn is over (or
                        // surface the error).
                        let agent = agent.clone();
                        let to_ws = to_ws_tx.clone();
                        let sid = session_id.clone();
                        let user_text = user_text.to_string();
                        tokio::spawn(async move {
                            let res = agent
                                .request(
                                    "session/prompt",
                                    json!({
                                        "sessionId": sid,
                                        "prompt": [{ "type": "text", "text": user_text }]
                                    })
                                )
                                .await;
                            if let Err(e) = res {
                                let _ = to_ws.send(text_msg(json!({ "type": "error", "message": format!("{e}") })));
                            }
                            let _ = to_ws.send(text_msg(json!({ "type": "prompt_done" })));
                        });
                    }
                    Some("permission_response") => {
                        // Browser replied to a `session/request_permission`
                        // we forwarded earlier. The `id` must match the one
                        // we forwarded; we pass it straight back to the
                        // agent so it can unblock.
                        let Some(id) = v.get("id").cloned() else {
                            continue;
                        };
                        let option_id = v
                            .get("optionId")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let agent = agent.clone();
                        let to_ws = to_ws_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = agent
                                .respond(
                                    id,
                                    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
                                )
                                .await
                            {
                                let _ = to_ws.send(text_msg(json!({
                                    "type": "error",
                                    "message": format!("permission reply failed: {e}")
                                })));
                            }
                        });
                    }
                    Some("cancel") => {
                        // ACP `session/cancel` is a notification (no id, no
                        // response expected). The agent is responsible for
                        // stopping whatever tool or turn is in flight and
                        // eventually resolving the outstanding
                        // `session/prompt` request, which is what unblocks
                        // the browser's "busy" state.
                        let agent = agent.clone();
                        let sid = session_id.clone();
                        tokio::spawn(async move {
                            let _ = agent
                                .notify(
                                    "session/cancel",
                                    json!({ "sessionId": sid })
                                )
                                .await;
                        });
                    }
                    Some("set_mode") => {
                        // Kiro calls them "modes" but the available ids are
                        // agent configs (`kiro_default`, `kiro_planner`,
                        // `kiro_guide`). Forward as `session/set_mode`.
                        let Some(mode_id) = v.get("modeId").and_then(Value::as_str) else {
                            continue;
                        };
                        let mode_id = mode_id.to_string();
                        let agent = agent.clone();
                        let sid = session_id.clone();
                        let to_ws = to_ws_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = agent
                                .request(
                                    "session/set_mode",
                                    json!({ "sessionId": sid, "modeId": mode_id })
                                )
                                .await
                            {
                                let _ = to_ws.send(text_msg(json!({
                                    "type": "error",
                                    "message": format!("set_mode failed: {e}")
                                })));
                            }
                        });
                    }
                    Some("set_model") => {
                        let Some(model_id) = v.get("modelId").and_then(Value::as_str) else {
                            continue;
                        };
                        let model_id = model_id.to_string();
                        let agent = agent.clone();
                        let sid = session_id.clone();
                        let to_ws = to_ws_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = agent
                                .request(
                                    "session/set_model",
                                    json!({ "sessionId": sid, "modelId": model_id })
                                )
                                .await
                            {
                                let _ = to_ws.send(text_msg(json!({
                                    "type": "error",
                                    "message": format!("set_model failed: {e}")
                                })));
                            }
                        });
                    }
                    _ => continue
                }
            }
            // Agent → user: notifications and server-initiated requests.
            Some(agent_msg) = updates_rx.recv() => {
                handle_agent_message(&to_ws_tx, agent_msg, suppress_session_updates).await;
            }
            else => break
        }
    }

    // Cooperative shutdown of the agent subprocess. Sends `session/cancel`,
    // closes stdin, and waits briefly for exit so Kiro can release its
    // session lock. `kill_on_drop` stays on as a safety net in case the
    // agent doesn't honour EOF within the timeout.
    agent.shutdown(Some(&session_id)).await;

    // Closing the outbound channel unblocks the writer task. The agent
    // child is killed on drop of the `Agent` (see `kill_on_drop(true)` in
    // `spawn_agent`) if shutdown timed out above.
    drop(to_ws_tx);
    let _ = writer.await;
    Ok(())
}

/// Translate an agent-originated message into browser-facing events.
///
/// `suppress_session_updates` is set by the WS handler during a resume
/// window: the browser seeds its log from `/history` instead of the ACP
/// replay, so forwarding `session/update` events would duplicate every
/// replayed chunk. Server-initiated requests (permission prompts) are
/// still forwarded — they only occur for live tool calls, not replay.
///
/// Currently understood:
///
/// - `session/update`:
///     - `agent_message_chunk`   → append as `agent` text
///     - `agent_thought_chunk`   → append as `sys` with a `(thinking)` prefix
///     - `tool_call` / `tool_call_update` → append `[title — status]`
/// - `session/request_permission` → forwarded to the browser as a
///   `permission_request` event.
/// - `_kiro.dev/commands/available` → trimmed and forwarded as a
///   `commands` event (just the `commands` + `prompts` arrays; the big
///   `tools` catalogue is dropped to keep the WS frame small).
///
/// Everything else is silently dropped, including Kiro's other
/// `_kiro.dev/*` extension notifications.
async fn handle_agent_message(
    tx: &mpsc::UnboundedSender<Message>,
    msg: Value,
    suppress_session_updates: bool
) {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "_kiro.dev/commands/available" => {
            // Kiro re-emits this notification as its catalogue warms up
            // (MCP servers load, etc.). We treat each emission as the
            // full current catalogue; last-wins semantics on the browser.
            if let Some(params) = msg.get("params") {
                let commands = params.get("commands").cloned().unwrap_or(Value::Array(vec![]));
                let prompts = params.get("prompts").cloned().unwrap_or(Value::Array(vec![]));
                let _ = tx.send(text_msg(json!({
                    "type": "commands",
                    "commands": commands,
                    "prompts": prompts
                })));
            }
        }
        "session/update" => {
            if suppress_session_updates {
                return;
            }
            let update = msg.get("params").and_then(|p| p.get("update")).cloned().unwrap_or(Value::Null);
            let kind = update.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
            match kind {
                "agent_message_chunk" => {
                    if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(Value::as_str) {
                        let _ = tx.send(text_msg(json!({ "type": "append", "role": "agent", "text": text })));
                    }
                }
                "user_message_chunk" => {
                    // Only emitted during `session/load` replay, so this
                    // does not double-render live prompts (the browser
                    // already echoes those locally).
                    if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(Value::as_str) {
                        let _ = tx.send(text_msg(json!({
                            "type": "append",
                            "role": "user",
                            "text": format!("> {text}\n")
                        })));
                    }
                }
                "agent_thought_chunk" => {
                    // Reasoning tokens. Kiro does not currently emit these,
                    // but leave the handler in place so reasoning-model
                    // agents light up the UI with `(thinking)` lines.
                    if let Some(text) = update.get("content").and_then(|c| c.get("text")).and_then(Value::as_str) {
                        let _ = tx.send(text_msg(json!({ "type": "append", "role": "sys", "text": format!("(thinking) {text}") })));
                    }
                }
                "tool_call" | "tool_call_update" => {
                    // Single-line "[title — status]" gives users visibility
                    // into long tool runs. Extend this to render structured
                    // tool IO if you want richer UI.
                    let title = update.get("title").and_then(Value::as_str).unwrap_or("tool");
                    let status = update.get("status").and_then(Value::as_str).unwrap_or("");
                    let line = if status.is_empty() {
                        format!("\n[{title}]\n")
                    } else {
                        format!("\n[{title} — {status}]\n")
                    };
                    let _ = tx.send(text_msg(json!({ "type": "append", "role": "sys", "text": line })));
                }
                _ => {}
            }
        }
        "session/request_permission" => {
            // Forward to the browser. The reply comes back as a
            // `permission_response` browser message, handled in the WS
            // select loop (see `handle_ws`). JSON-RPC id is passed through
            // unchanged so we can respond to the agent with it.
            if let Some(params) = msg.get("params") {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let title = params
                    .get("toolCall")
                    .and_then(|tc| tc.get("title").or_else(|| tc.get("name")))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let options = params.get("options").cloned().unwrap_or(Value::Array(vec![]));
                let _ = tx.send(text_msg(json!({
                    "type": "permission_request",
                    "id": id,
                    "title": title,
                    "options": options
                })));
            }
        }
        _ => {
            // Unhandled method: Kiro extensions like `_kiro.dev/commands/available`
            // land here. Add arms as needed.
        }
    }
}

// ---------- helpers ----------

/// Serialise a JSON value into a WS text frame. The terminology split keeps
/// `handle_agent_message` free of `Message::Text(...)` noise.
fn text_msg(value: Value) -> Message {
    Message::Text(value.to_string())
}

/// Pull the `modes` and `models` blocks out of a `session/new` or
/// `session/load` result. Returns `None` when neither is present so the
/// WS handler can skip emitting the `session_info` event entirely.
///
/// The shape passed through is exactly what Kiro sends, so the browser
/// can key off `currentModeId` / `availableModes` / `currentModelId` /
/// `availableModels` without any translation.
fn extract_session_info(result: &Value) -> Option<Value> {
    let modes = result.get("modes").cloned();
    let models = result.get("models").cloned();
    if modes.is_none() && models.is_none() {
        return None;
    }
    Some(json!({
        "modes": modes,
        "models": models
    }))
}

// ---------- stale-lock recovery ----------
//
// Kiro writes a `<session-id>.lock` file into `~/.kiro/sessions/cli/` while
// an ACP process is attached to that session. Two ways this gets in our
// way:
//
// 1. Dead-PID stale lock. A previous Okiro (or Kiro child) was SIGKILLed
//    before its cooperative shutdown could run. The lockfile persists
//    pointing at a PID that no longer exists.
// 2. Live-PID transient contention. Browser reload causes the old WS
//    handler to begin shutting down Kiro while the new WS handler is
//    already trying to `session/load`. For a few hundred ms the old Kiro
//    really is alive and really does own the session.
//
// `try_load_session` below handles both: it retries `session/load` with a
// short back-off while the error is "Session is active in another
// process", stealing the lockfile whenever the named PID is dead.

/// Attempt to resume an existing ACP session, recovering from the stale
/// lock / shutdown-race conditions described above.
///
/// On success returns the full `session/load` result so the caller can
/// forward modes/models to the browser just like on `session/new`. On a
/// non-recoverable error, or if retries are exhausted, returns
/// `Err(last_error_message)`.
async fn try_load_session(agent: &Agent, sid: &str, cwd: &str) -> std::result::Result<Value, String> {
    // ~1.25s total budget: 5 attempts at 250ms spacing. Empirically covers
    // the cooperative shutdown path (500ms) plus a little headroom for
    // Kiro to actually release the lockfile after its child exits.
    const ATTEMPTS: u32 = 6;
    const BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

    let mut last_err = String::new();
    for attempt in 0..ATTEMPTS {
        let res = agent
            .request(
                "session/load",
                json!({
                    "sessionId": sid,
                    "cwd": cwd,
                    "mcpServers": []
                })
            )
            .await;
        match res {
            Ok(value) => return Ok(value),
            Err(e) => {
                last_err = format!("{e}");
                if !is_stale_lock_error(&last_err) {
                    // Any other error (session truly missing, schema
                    // mismatch, etc.) is not going to fix itself with
                    // retries. Give up immediately.
                    break;
                }
                // Steal the lock if its PID is dead; this always makes
                // the next attempt succeed if the problem was purely a
                // stale lockfile. If the PID is alive we leave the lock
                // alone and let the shutdown-race back-off do its job.
                let stole = steal_stale_session_lock(sid);
                if stole {
                    eprintln!("session/load {sid}: stale lock stolen on attempt {}", attempt + 1);
                    // Don't burn a backoff sleep if we just cleared the
                    // blocker ourselves.
                    continue;
                }
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(BACKOFF).await;
                }
            }
        }
    }
    Err(last_err)
}

/// True if the agent's error message is the stale-PID lock case.
fn is_stale_lock_error(msg: &str) -> bool {
    msg.contains("Session is active in another process")
}

/// If the lockfile for `session_id` points at a dead PID, remove it and
/// return true. Any uncertainty (lockfile missing, unreadable, malformed,
/// PID still alive) returns false so we fall through to `session/new`.
fn steal_stale_session_lock(session_id: &str) -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let path = std::path::PathBuf::from(home).join(format!(".kiro/sessions/cli/{session_id}.lock"));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    // Lockfile shape: {"pid":12345,"started_at":"..."}. Keep the parse
    // narrow; we don't care about the timestamp.
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    let Some(pid) = parsed.get("pid").and_then(Value::as_i64) else {
        return false;
    };
    if pid_is_alive(pid as i32) {
        return false;
    }
    match std::fs::remove_file(&path) {
        Ok(()) => {
            eprintln!("stole stale Kiro session lock (pid {pid}): {}", path.display());
            true
        }
        Err(_) => false
    }
}

/// Unix PID liveness check. `kill(pid, 0)` returns 0 if the process exists
/// and we can signal it, `-1` otherwise. On ESRCH (no such process) the
/// PID is definitely dead; on EPERM the process exists but we can't
/// signal it, which for our case means we should NOT steal the lock.
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: `kill` with signal 0 does not send a signal, it only
    // queries existence. No state is mutated.
    unsafe { libc_kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    // Non-unix: don't risk stealing a lock we can't verify.
    true
}

// Minimal FFI binding to avoid pulling in `libc` for one call.
#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// First eight hex chars of a UUID-shaped session id, for user-facing log.
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Best-effort one-liner summary of an agent error string for the log.
fn short_reason(msg: &str) -> String {
    // Strip our wrapper and the JSON framing Kiro returns. The interesting
    // bits live in the `data` field of the JSON-RPC error.
    if let Some(start) = msg.find("\"data\":\"") {
        let rest = &msg[start + 8..];
        if let Some(end) = rest.find('"') {
            return rest[..end].to_string();
        }
    }
    // Fallback: trim the generic prefixes so the user sees something
    // useful rather than three nested quote levels.
    msg.trim_start_matches("agent error: ").trim().to_string()
}

// ---------- telegram transport (stub) ----------
//
// TODO(telegram): implement. Planned shape:
//   - Long-poll https://api.telegram.org/bot<token>/getUpdates.
//   - One ACP agent per Telegram chat, spawned on first message, torn down
//     on idle timeout.
//   - Stream `agent_message_chunk` output as a single `sendMessage` followed
//     by `editMessageText` calls throttled to ~1/s (Telegram per-chat limit).
//   - Inline keyboard for `session/request_permission` replies.
//   - Per-user bot tokens (created via @BotFather) keep Okiro out of the
//     shared infrastructure path; long polling requires exactly one process
//     per token.

async fn run_telegram(_cfg: Config) -> Result<()> {
    bail!("telegram transport is not yet implemented; re-run `okiro init` and pick cloudflared");
}

// ---------- ACP agent subprocess ----------
//
// One `Agent` wraps one spawned child process and its JSON-RPC framing.
// The stdout reader task (see `spawn_agent`) splits incoming traffic into
// two streams:
//   - **Responses** (messages with `result` or `error` and a known `id`)
//     are delivered to the matching oneshot sender registered by `request`.
//   - **Notifications and server-initiated requests** go out through the
//     `updates_rx` mpsc channel, which the WS handler drains.

/// Handle on the ACP agent subprocess.
///
/// Thread-safety: all mutable state is behind `Mutex`/`Arc`, so the handle
/// can be cloned into spawned tasks (as `Arc<Agent>` in `handle_ws`).
struct Agent {
    /// Stdin to the child; serialised by a Mutex because prompt tasks may
    /// try to write concurrently.
    stdin: Mutex<ChildStdin>,
    /// Monotonic JSON-RPC id generator.
    next_id: AtomicI64,
    /// Map from in-flight request id to the oneshot waiting for its
    /// response. Shared with the reader task that populates responses.
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    /// Updates receiver, handed out exactly once via `take_updates`.
    updates_rx: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
    /// Owned child. SIGKILL on drop (kill_on_drop) remains as a safety net,
    /// but `shutdown` tries a clean EOF+wait first so Kiro can release its
    /// per-session lockfile.
    child: Mutex<Child>
}

impl Agent {
    /// Extract the updates receiver. Must only be called once per Agent.
    fn take_updates(&self) -> mpsc::UnboundedReceiver<Value> {
        self.updates_rx
            .try_lock()
            .expect("updates_rx already locked")
            .take()
            .expect("updates_rx already taken")
    }

    /// Send a JSON-RPC request and await its response.
    ///
    /// Returns the `result` value on success, or an error if the agent
    /// responded with `error`, closed before replying, or the stdin write
    /// failed. The caller is responsible for cancellation semantics — if
    /// the future is dropped mid-flight, the response will arrive at a
    /// dangling oneshot and be discarded.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let line = format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        {
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await?;
        }

        let resp = rx.await.context("agent closed before replying")?;
        if let Some(err) = resp.get("error") {
            bail!("agent error: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Reply to a server-initiated request (e.g. `session/request_permission`).
    async fn respond(&self, id: Value, result: Value) -> Result<()> {
        let line = format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Send a JSON-RPC notification (no id, no response expected). Used for
    /// one-way signals like `session/cancel`.
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let line = format!("{}\n", json!({ "jsonrpc": "2.0", "method": method, "params": params }));
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Cooperative shutdown:
    ///   1. Best-effort `session/cancel` so any in-flight tool or turn stops.
    ///   2. Close stdin so the agent sees EOF and exits cleanly. Kiro uses
    ///      this signal to release its per-session PID lockfile; without it
    ///      you get "Session is active in another process (PID ...)" errors
    ///      on the next `session/load`.
    ///   3. Wait up to 500ms for the child to exit.
    ///   4. If the timeout expires, fall through — `kill_on_drop` will still
    ///      SIGKILL the child when the Agent is dropped shortly after.
    async fn shutdown(&self, session_id: Option<&str>) {
        if let Some(sid) = session_id {
            let _ = self.notify("session/cancel", json!({ "sessionId": sid })).await;
        }
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
            let mut child = self.child.lock().await;
            let _ = child.wait().await;
        })
        .await;
    }
}

/// Spawn the configured agent and wire its stdio into the `Agent` handle.
///
/// The child is configured with `kill_on_drop(true)` so that when the
/// returned `Agent` (or an `Arc` containing it) is dropped, the process
/// goes with it — important because each browser session owns its own
/// agent and we do not want orphan children hanging around.
///
/// Two background tasks are spawned here:
///   1. Stderr forwarder — writes each line to our stderr prefixed with
///      `[agent]`, for debugging.
///   2. Stdout reader — newline-delimited JSON decoder that routes
///      responses to their pending oneshots and everything else to the
///      `updates_tx` mpsc.
async fn spawn_agent(cfg: &Config) -> Result<Agent> {
    let mut cmd = Command::new(&cfg.agent_cmd);
    cmd.args(&cfg.agent_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{}`", cfg.agent_cmd))?;

    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    // Stderr forwarder.
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("[agent] {line}");
        }
    });

    let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
    let (updates_tx, updates_rx) = mpsc::unbounded_channel();

    // Optional ACP tracing. Set `OKIRO_DEBUG_ACP=1` to dump every inbound
    // line from the agent to Okiro's stderr. Helpful when wiring new
    // Kiro extensions (`_kiro.dev/*`) or debugging wire-shape mismatches.
    let debug_acp = std::env::var_os("OKIRO_DEBUG_ACP").is_some();

    // Stdout reader: route responses vs notifications.
    //
    // A response is any message carrying `result` or `error` whose `id`
    // matches a pending request we sent. Everything else — notifications
    // (no id) and server-initiated requests (id but no result/error) — is
    // pushed onto the updates channel for the WS handler to act on.
    let pending_reader = pending.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if debug_acp {
                eprintln!("[acp<-] {line}");
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue // malformed line; skip silently
            };
            let is_response = msg.get("result").is_some() || msg.get("error").is_some();
            if is_response {
                if let Some(id) = msg.get("id").and_then(Value::as_i64) {
                    if let Some(tx) = pending_reader.lock().await.remove(&id) {
                        let _ = tx.send(msg);
                        continue;
                    }
                }
            }
            let _ = updates_tx.send(msg);
        }
    });

    Ok(Agent {
        stdin: Mutex::new(stdin),
        next_id: AtomicI64::new(1),
        pending,
        updates_rx: Mutex::new(Some(updates_rx)),
        child: Mutex::new(child)
    })
}
