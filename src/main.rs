//! racp — bridge an ACP-compliant local agent to a remote UI.
//!
//! # Overview
//!
//! racp is an **ACP client**. For each incoming browser WebSocket it spawns a
//! fresh agent subprocess (configured via `agent_cmd` + `agent_args`), sends
//! `initialize` followed by `session/new`, then forwards each user message as
//! `session/prompt` and streams `session/update` notifications back to the
//! browser as terminal-style text.
//!
//! One WebSocket connection = one agent subprocess = one ACP session. When the
//! browser disconnects the agent is killed (via `kill_on_drop(true)`).
//!
//! # Transports
//!
//! - `cloudflared`: HTTP + WebSocket on `127.0.0.1:<bind>` fronted by an
//!   existing Cloudflare Tunnel. The terminal-style UI is embedded via
//!   `include_str!("ui.html")` so the binary is self-contained.
//! - `telegram`: stub. See [`run_telegram`]; schema is stable so implementing
//!   it later does not break existing configs.
//!
//! # Wire shapes
//!
//! Browser ↔ racp (JSON over WS):
//!   { type: "prompt", text: string }                           // client→server
//!   { type: "append", role: "user"|"agent"|"sys", text: str }  // server→client
//!   { type: "prompt_done" }                                    // server→client
//!   { type: "error", message: string }                         // server→client
//!
//! racp ↔ agent (JSON-RPC 2.0 over stdio): standard ACP. See the README for
//! the list of methods and `session/update` variants we actually handle, and
//! which ones Kiro emits vs ignores.
//!
//! # Known gaps
//!
//! Search for `TODO:` to find the extension points. The README has the same
//! list with more context for new contributors.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State
    },
    http::StatusCode,
    response::{Html, Response},
    routing::get,
    Json, Router
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};

const PROTOCOL_VERSION: u32 = 1;
const UI_HTML: &str = include_str!("ui.html");

// ---------- config ----------
//
// On-disk config lives at `~/.racp/config.toml`. Schema changes are breaking
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
    Ok(PathBuf::from(home).join(".racp/config.toml"))
}

/// Path to the persistent browser state (currently-open tabs, history list,
/// active id, next numeric label). Server-side so any device hitting racp
/// sees the same list.
fn state_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".racp/state.json"))
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
        .route("/", get(serve_ui))
        .route("/ws", get(ws_upgrade))
        .route("/state", get(get_state).put(put_state))
        .with_state(shared);

    let listener = TcpListener::bind(&cfg.bind).await?;
    eprintln!("racp listening on http://{} (front with your Cloudflare Tunnel)", cfg.bind);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve_ui() -> Html<&'static str> {
    Html(UI_HTML)
}

/// GET /state — returns the persisted browser state as JSON, or `{}` if the
/// file does not exist yet. racp does not interpret the contents; it is
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

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    State(cfg): State<Arc<Config>>
) -> Response {
    // `/ws?session=<acp-session-id>` asks racp to call `session/load` on the
    // agent instead of `session/new`. Absent = always new session.
    // `/ws?cwd=<path>` overrides the working directory for this session;
    // absent or empty = racp's own process cwd.
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
    // because racp does not back `fs/read_text_file` etc. today — the agent
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
    // otherwise we use racp's own process cwd.
    let cwd_str = match cwd_override {
        Some(c) => c,
        None => std::env::current_dir()?.to_string_lossy().to_string()
    };

    let (session_id, resumed) = match resume_session_id {
        Some(sid) => {
            let load_res = agent
                .request(
                    "session/load",
                    json!({
                        "sessionId": sid,
                        "cwd": cwd_str,
                        "mcpServers": []
                    })
                )
                .await;
            match load_res {
                Ok(_) => (sid, true),
                Err(e) => {
                    eprintln!("session/load failed ({e}); falling back to session/new");
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
                    (sid, false)
                }
            }
        }
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
            (sid, false)
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
                    _ => continue
                }
            }
            // Agent → user: notifications and server-initiated requests.
            Some(agent_msg) = updates_rx.recv() => {
                handle_agent_message(&to_ws_tx, agent_msg).await;
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
/// Currently understood:
///
/// - `session/update`:
///     - `agent_message_chunk`   → append as `agent` text
///     - `agent_thought_chunk`   → append as `sys` with a `(thinking)` prefix
///     - `tool_call` / `tool_call_update` → append `[title — status]`
/// - `session/request_permission` → auto-allow the first offered option
///
/// Everything else is silently dropped, including Kiro's `_kiro.dev/*`
/// extension notifications (slash-command catalogue, MCP OAuth URLs, etc.).
/// Wire those up here if you want them surfaced — each has its own
/// `method` name and params shape; see the Kiro CLI ACP docs.
///
/// TODO(permission-ui): replace the auto-allow with a real prompt. The browser
/// protocol needs a new message type carrying the toolCall summary and the
/// option list, and a corresponding response type selecting an option.
async fn handle_agent_message(tx: &mpsc::UnboundedSender<Message>, msg: Value) {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    match method {
        "session/update" => {
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

// ---------- telegram transport (stub) ----------
//
// TODO(telegram): implement. Planned shape:
//   - Long-poll https://api.telegram.org/bot<token>/getUpdates.
//   - One ACP agent per Telegram chat, spawned on first message, torn down
//     on idle timeout.
//   - Stream `agent_message_chunk` output as a single `sendMessage` followed
//     by `editMessageText` calls throttled to ~1/s (Telegram per-chat limit).
//   - Inline keyboard for `session/request_permission` replies.
//   - Per-user bot tokens (created via @BotFather) keep racp out of the
//     shared infrastructure path; long polling requires exactly one process
//     per token.

async fn run_telegram(_cfg: Config) -> Result<()> {
    bail!("telegram transport is not yet implemented; re-run `racp init` and pick cloudflared");
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
